//! TEA 事件循环 + update + Command 执行。
//! 所有并发事件源汇成单一 mpsc<Msg>,主循环逐条喂 update。

use crate::backend::{self, resolve_mcp_config, RunSpec};
use crate::issue;
use crate::model::*;
use crate::tui;
use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// 执行副作用的运行时环境(不含 Model,便于在借用 Model 的同时调用)。
pub struct Runtime {
    tx: UnboundedSender<Msg>,
}

impl Runtime {
    pub fn new(tx: UnboundedSender<Msg>) -> Self {
        Runtime { tx }
    }
    pub fn exec(&self, model: &Model, cmd: Command) {
        execute(&self.tx, model, cmd);
    }
}

/// 纯逻辑:处理一条 Msg,更新 Model,产出副作用 Command 列表。
pub fn update(m: &mut Model, msg: Msg) -> Vec<Command> {
    match msg {
        Msg::Tick => {
            m.tick = m.tick.wrapping_add(1);
            // status_hint 到期自动清
            if m.status_hint.is_some() && m.tick >= m.status_hint_until {
                m.status_hint = None;
            }
            // pending_delete 窗口过期自动清
            if let Some((_, t0)) = m.pending_delete {
                if m.tick.wrapping_sub(t0) >= 33 {
                    m.pending_delete = None;
                }
            }
            vec![]
        }
        Msg::Key(k) => handle_key(m, k),
        Msg::Select(sel) => {
            let valid = match sel {
                Selection::Chat => true,
                Selection::Member(i) => i < m.members.len(),
            };
            if valid {
                m.selection = sel;
                m.scroll = 0;
            }
            vec![]
        }
        Msg::MouseTabClick { col } => {
            // 保守 hit-test:重现 draw_tabs 的 span 布局,算命中
            handle_tab_click(m, col);
            vec![]
        }
        Msg::AgentStdout { name, line } => {
            if let Some(i) = m.member_index(&name) {
                let mem = &mut m.members[i];
                if mem.state == AgentState::Thinking {
                    mem.state = AgentState::Working;
                }
                mem.push_raw(line);
            }
            vec![]
        }
        Msg::AgentDone { name, issue, gen, worktree, full_output, ok, err } => {
            handle_agent_done(m, name, issue, gen, worktree, full_output, ok, err)
        }
        Msg::IoError { detail } => {
            // 落盘失败必须让用户看见 —— 否则他会以为历史都保住了
            set_hint(m, format!("⚠ 落盘失败:{detail}"), 12);
            vec![]
        }
    }
}

fn handle_key(m: &mut Model, k: crossterm::event::KeyEvent) -> Vec<Command> {
    if k.kind == KeyEventKind::Release {
        return vec![];
    }
    // ? 键切换帮助浮层(在任何状态下都可用,除非正在打字里输入 ?)
    if matches!(k.code, KeyCode::Char('?')) && !k.modifiers.contains(KeyModifiers::CONTROL) {
        // 只有输入框为空时,? 打开帮助;有内容则视为普通字符
        if m.input.is_empty() {
            m.show_help = !m.show_help;
            if m.show_help {
                m.status_hint = Some("? / Esc 关闭帮助".into());
            } else {
                m.status_hint = None;
            }
            return vec![];
        }
    }
    // 帮助浮层打开时,? 切换关闭,Esc 关闭并回群聊,其他键关闭并传递
    if m.show_help {
        if matches!(k.code, KeyCode::Esc) {
            m.show_help = false;
            m.selection = Selection::Chat;
            m.scroll = 0;
            return vec![];
        }
        if matches!(k.code, KeyCode::Char('?')) {
            m.show_help = false;
            return vec![];
        }
        // 其他键(退格、字母…)关闭帮助并继续处理
        m.show_help = false;
    }

    // status_hint 会在下面被清:见「_tick 自动过期」;这里不再无条件清

    // Alt+数字:切议题(比 Ctrl+数字 兼容性好——不少终端根本不发 Ctrl+数字)
    if k.modifiers.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c) = k.code {
            if let Some(d) = c.to_digit(10) {
                let n = d as usize;
                if n >= 1 && n <= m.issues.len() {
                    m.current_issue = n - 1;
                    m.selection = Selection::Chat;
                    m.scroll = 0;
                    set_hint(m, format!("切到议题 {n}"), 5);
                }
            }
            // Alt+其它字符一律吞掉:不能让 Alt+a 落下去被当成普通输入插进输入框
            return vec![];
        }
    }

    // Ctrl 组合
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('c') => {
                // 有 agent 在跑:先掐掉它们再说。这些进程跑的是 bypass 权限,
                // 直接退出会把它们留成孤儿,继续在工作区里改文件。
                let running = m.working_count();
                if running > 0 {
                    m.cancel.cancel();
                    // 换一个新 token,否则之后新起的 agent 一生下来就是取消态
                    m.cancel = tokio_util::sync::CancellationToken::new();
                    // 排队的活也得清:不清的话被取消的成员一交卷就走 drain_inbox,
                    // 拿着**新** token 立刻起下一个进程,用户按不停也按不出去。
                    let queued: usize = m.members.iter().map(|x| x.inbox.len()).sum();
                    for mem in &mut m.members {
                        mem.inbox.clear();
                    }
                    let extra = if queued > 0 {
                        format!("、清掉 {queued} 条排队任务")
                    } else {
                        String::new()
                    };
                    set_hint(m, format!("已取消 {running} 个在跑的 agent{extra};再按 ^C 退出"), 8);
                } else {
                    m.should_quit = true;
                }
                return vec![];
            }
            // 有些终端把退格发成 Ctrl+H
            KeyCode::Char('h') => {
                m.input.pop();
                return vec![];
            }
            // Ctrl+U 清空输入行
            KeyCode::Char('u') => {
                m.input.clear();
                return vec![];
            }
            KeyCode::Char('p') => {
                // 恢复暂停,并把暂停期间排队的活放出来
                //(以前只清 paused 就完事,排队的任务得等下一次凑巧才被想起来)
                m.cur_issue_mut().paused = false;
                m.cur_issue_mut().chain_depth = 0;
                let cmds = drain_all_inboxes(m);
                let n = cmds.len();
                if n > 0 {
                    set_hint(m, format!("已恢复,放出 {n} 条排队任务"), 5);
                } else {
                    set_hint(m, "已恢复", 5);
                }
                return cmds;
            }
            KeyCode::Char('n') => {
                // 直接建一个新议题,名字自动递增,并切过去
                let name = next_issue_name(&m.issues);
                m.issues.push(Issue::new(name.clone()));
                m.current_issue = m.issues.len() - 1;
                m.selection = Selection::Chat;
                m.scroll = 0;
                m.input.clear();
                set_hint(m, format!("已建议题:{name}"), 5);
                m.pending_delete = None;
                return vec![];
            }
            KeyCode::Char('w') => {
                // 关闭当前议题(有内容需二次确认)
                return handle_close_issue(m);
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap() as usize;
                if n >= 1 && n <= m.issues.len() {
                    m.current_issue = n - 1;
                    m.selection = Selection::Chat;
                    m.scroll = 0;
                    set_hint(m, format!("切到议题 {n}"), 5);
                } else {
                    set_hint(m, format!("^{n}:超出议题数({}/{})", n, m.issues.len()), 5);
                }
                return vec![];
            }
            _ => return vec![],
        }
    }

    match k.code {
        KeyCode::Esc => {
            m.selection = Selection::Chat;
            m.scroll = 0;
        }
        KeyCode::Up => {
            move_selection(m, -1);
        }
        KeyCode::Down => {
            move_selection(m, 1);
        }
        KeyCode::PageUp => {
            m.scroll = m.scroll.saturating_add(5);
        }
        KeyCode::PageDown => {
            m.scroll = m.scroll.saturating_sub(5);
        }
        KeyCode::Enter => {
            return submit_input(m);
        }
        KeyCode::Tab => {
            // Tab 补全:把正在输入的 @token 补成第一个建议
            let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
            let sugg = at_suggestions(&m.input, &roster);
            if let Some(first) = sugg.first() {
                if let Some(at) = m.input.rfind('@') {
                    m.input.truncate(at + '@'.len_utf8());
                    m.input.push_str(first);
                    m.input.push(' ');
                }
            }
        }
        KeyCode::Backspace | KeyCode::Delete => {
            m.input.pop();
        }
        // 某些终端把退格发成 DEL(0x7f)或 BS(0x08)字符
        KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => {
            m.input.pop();
        }
        KeyCode::Char(c) => {
            m.input.push(c);
        }
        _ => {}
    }
    vec![]
}

fn move_selection(m: &mut Model, delta: i32) {
    // 顺序:Chat(=-1 索引) 然后 Member 0..n
    let cur = match m.selection {
        Selection::Chat => -1,
        Selection::Member(i) => i as i32,
    };
    let n = m.members.len() as i32;
    let next = (cur + delta).clamp(-1, n - 1);
    m.selection = if next < 0 {
        Selection::Chat
    } else {
        Selection::Member(next as usize)
    };
    m.scroll = 0;
}

/// 我按下回车:发一条我消息;带 @ 才派活。
fn submit_input(m: &mut Model) -> Vec<Command> {
    let text = m.input.trim().to_string();
    if text.is_empty() {
        return vec![];
    }
    m.input.clear();

    // 斜杠命令(目前只有 /team,直接切队)
    if let Some(slash) = crate::slash::parse(&text) {
        return handle_slash(m, slash);
    }

    let mut cmds = Vec::new();

    // 自动取名:议题名是「议题N」这种自动生成的、且时间线为空(只可能有系统欢迎消息也算)
    // 用当前消息前 20 字符作新名字,把旧的 jsonl(如果存在)删掉
    let is_auto_name = m.cur_issue().name.starts_with("议题")
        && m.cur_issue().name[6..].chars().all(|c| c.is_ascii_digit())
        || m.cur_issue().name == "默认议题";
    let only_system_msgs = m.cur_issue().timeline.iter().all(|c| c.is_system);
    if is_auto_name && only_system_msgs {
        let new_name = derive_issue_name(&text, &m.issues, m.current_issue);
        if new_name != m.cur_issue().name {
            let old_name = std::mem::replace(&mut m.cur_issue_mut().name, new_name.clone());
            // 改名而不是删掉:旧文件里可能已经落了内容(比如自动改名前的
            // 「X 掉线」系统消息),直接删会把它永久丢掉,重启后就没了。
            cmds.push(Command::RenameIssueFile { from: old_name, to: new_name });
        }
    }

    let issue_name = m.cur_issue().name.clone();

    // 记入时间线 + 落盘
    let msg = ChatMsg {
        ts: now_ts(),
        author: "我".into(),
        text: text.clone(),
        is_system: false,
    };
    m.cur_issue_mut().timeline.push(msg.clone());
    cmds.push(Command::PersistChat { issue: issue_name.clone(), msg });

    // 我指令:重置连锁深度、解除暂停
    m.cur_issue_mut().chain_depth = 0;
    m.cur_issue_mut().paused = false;

    // 解析 @ —— 不带 @ 则只是留言,不触发任何人
    let issue_id = m.cur_issue().id;
    let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let mentions = crate::router::parse_owner_mentions(&text, &roster);
    for name in mentions {
        let assignment = format!("[我] {text}");
        if let Some(c) = dispatch(m, issue_id, &name, assignment, 0) {
            cmds.push(c);
        }
    }
    cmds
}

/// 一轮结束:提取汇报入群聊、落盘、解析 @ 继续派活、处理排队。
///
/// `issue_id` 是派活时绑定的议题。**不能**用「当前选中议题」—— agent 干一轮要几十秒到几分钟,
/// 这期间用户很可能切了 tab 甚至关掉原议题,那样汇报会写进别人的时间线和 jsonl。
#[allow(clippy::too_many_arguments)] // 都是 AgentDone 的字段,拆结构体反而更绕
fn handle_agent_done(
    m: &mut Model,
    name: String,
    issue_id: u64,
    gen: u64,
    worktree: Option<(std::path::PathBuf, String)>,
    full_output: String,
    ok: bool,
    err: Option<String>,
) -> Vec<Command> {
    let mut cmds = Vec::new();

    // 团队已经热切过了:这条结果属于旧花名册。按新花名册解析它的 @ 会把
    // 新团队里同名的人莫名唤醒,所以整条作废(状态也不用清,成员对象已经换掉了)。
    if gen != m.team_gen {
        set_hint(m, format!("{name} 的结果属于已切换的团队,已丢弃"), 8);
        return cmds;
    }

    // 该成员回到空闲
    if let Some(i) = m.member_index(&name) {
        m.members[i].state = AgentState::Idle;
    }

    // 议题已被关掉:汇报无处可去。只给一条临时提示,绝不写进别的议题。
    let Some(idx) = m.issue_index(issue_id) else {
        set_hint(m, format!("{name} 的汇报所属议题已关闭,已丢弃"), 8);
        cmds.extend(drain_inbox(m, &name));
        return cmds;
    };
    let issue_name = m.issues[idx].name.clone();
    let depth_when_started = m.issues[idx].chain_depth;

    if !ok {
        // 用户主动取消 与 真掉线 分开措辞,别把自己按的 ^C 说成「掉线」
        let reason = err.unwrap_or_else(|| "未知错误".into());
        let mut text = if reason == CANCELLED {
            format!("{name} 已被取消")
        } else {
            format!("{name} 掉线:{reason}")
        };
        // 失败/取消时更容易留下空 worktree:没动过就回收,动过就告诉用户在哪
        if let Some((wt_dir, branch)) = &worktree {
            if wt_dir.exists() && !crate::worktree::drop_if_untouched(&m.work_dir, wt_dir, branch) {
                let summary = crate::worktree::change_summary(wt_dir, &m.work_dir, branch);
                text.push_str(&format!("(已改的留在 {branch} — {summary})"));
            }
        }
        let msg = ChatMsg { ts: now_ts(), author: "系统".into(), text, is_system: true };
        m.issues[idx].timeline.push(msg.clone());
        cmds.push(Command::PersistChat { issue: issue_name.clone(), msg });
        // 尝试处理该成员排队的下一条
        cmds.extend(drain_inbox(m, &name));
        return cmds;
    }

    // 提取汇报(完整,不截断)
    let report = crate::router::extract_report(&full_output);

    // 先解析 @ ——必须在截断之前。团队规约要求 agent「在结尾 @下一个人」,
    // 先截断的话尾部的 @ 会被一起切掉,接力链断在第一跳且毫无提示。
    let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let mentions = crate::router::parse_mentions(&report, &roster, &name);

    // 群聊里展示/落盘的是截断版(若真截断了会标注派给了谁)
    let mut chat_text = crate::router::report_for_chat(&report, &mentions);

    // 用了 worktree 就附上分支名和改动摘要。这里用的是**派活时回投**的路径,
    // 不是去磁盘上按时间猜 —— 猜会拿到别轮甚至别的 agent 的 worktree。
    //
    // `upstream_branch` 会写进给接力者的指派里 —— 接力者在自己的 worktree 里,
    // 不显式告诉它上游分支名和怎么用,它就得靠 git branch -a 瞎找(碰运气才成)。
    let mut upstream_branch: Option<String> = None;
    if let Some((wt_dir, branch)) = &worktree {
        if wt_dir.exists() {
            // 一点改动都没有(纯查询类任务)→ 直接回收,别留个完整 checkout 占磁盘
            if crate::worktree::drop_if_untouched(&m.work_dir, wt_dir, branch) {
                // 什么都不追加:没改动就没什么可让用户采纳的
            } else {
                upstream_branch = Some(branch.clone());
                let summary = crate::worktree::change_summary(wt_dir, &m.work_dir, branch);
                chat_text.push_str(&format!("\n📂 {branch} — {summary}"));
            }
        }
    }

    let msg = ChatMsg { ts: now_ts(), author: name.clone(), text: chat_text, is_system: false };
    m.issues[idx].timeline.push(msg.clone());
    cmds.push(Command::PersistChat { issue: issue_name.clone(), msg });

    // 防乒乓:连锁深度 +1
    let new_depth = depth_when_started + 1;
    if new_depth > m.max_chain_depth {
        m.issues[idx].paused = true;
        let text = format!(
            "@ 连锁已达 {new_depth} 轮,自动暂停以防打转。按 Ctrl+P 恢复,或直接发新指令。"
        );
        let smsg = ChatMsg { ts: now_ts(), author: "系统".into(), text, is_system: true };
        m.issues[idx].timeline.push(smsg.clone());
        cmds.push(Command::PersistChat { issue: issue_name, msg: smsg });
        // 这一轮解析出的 @ 同样不能丢:议题已 paused,dispatch 会把它们入队,
        // 等用户按 ^P 再放出来。以前这里直接 return,接力链无声蒸发,
        // 而系统消息还在教用户「按 Ctrl+P 恢复」—— 按了也只会得到「已恢复」。
        for target in mentions {
            let assignment = handoff_text(&name, &report, upstream_branch.as_deref());
            if let Some(c) = dispatch(m, issue_id, &target, assignment, new_depth) {
                cmds.push(c);
            }
        }
        return cmds;
    }
    m.issues[idx].chain_depth = new_depth;

    // 解析出的 @ 逐个派活(投递用**完整**汇报,不能把截断版喂给下游)
    for target in mentions {
        let assignment = handoff_text(&name, &report, upstream_branch.as_deref());
        if let Some(c) = dispatch(m, issue_id, &target, assignment, new_depth) {
            cmds.push(c);
        }
    }

    // 该成员自己的排队任务
    cmds.extend(drain_inbox(m, &name));
    cmds
}

/// 拼给接力者的指派文本。
///
/// 上游用了 worktree 的话必须**显式**告诉接力者分支名和该怎么用 ——
/// 接力者在自己的 worktree 里,初始内容是主分支的旧代码。不说的话它只能靠
/// `git branch -a` 瞎猜,猜到算运气好,猜不到就在旧代码上做决策(评审一份
/// 不存在的代码、在没改过的文件上继续改)。
fn handoff_text(from: &str, report: &str, upstream_branch: Option<&str>) -> String {
    let base = format!("[来自 {from}] {report}");
    match upstream_branch {
        Some(b) => format!(
            "{base}\n\n[上游改动] {from} 的改动已提交在分支 `{b}`。\
你现在在自己的 worktree 里,看到的还是改动前的代码 —— \
先 `git merge {b}` 把它并进来(要看差异用 `git diff HEAD {b}`),再开始你这一轮的工作。"
        ),
        None => base,
    }
}

/// 派活给某成员。议题暂停或成员忙 → 入队;闲则起进程。返回 SpawnAgent 命令(如需)。
/// 任何情况下都**不丢**派活:以前 paused 时直接 return,那条活既不入队也不留痕,凭空消失。
fn dispatch(
    m: &mut Model,
    issue_id: u64,
    name: &str,
    assignment: String,
    chain_depth: u32,
) -> Option<Command> {
    let i = m.member_index(name)?;
    let idx = m.issue_index(issue_id)?; // 议题已关闭 → 无处可派
    if m.issues[idx].paused || m.members[i].state != AgentState::Idle {
        // 暂停中 或 忙 → 排队(暂停的等 Ctrl+P 放出来)
        let dropped = m.members[i]
            .push_inbox(Assignment { issue: issue_id, text: assignment });
        if dropped.is_some() {
            // 丢了活必须说,别让它静默消失
            set_hint(m, format!("{name} 待办已满({INBOX_CAP} 条),丢弃了最旧的一条"), 10);
        }
        return None;
    }
    // 闲 → 起进程
    let timeline = m.issues[idx].timeline.clone();
    let user_input = issue::build_prompt_input(issue_id, &timeline, &m.members[i], &assignment);
    m.members[i].last_seen.insert(issue_id, timeline.len());
    m.members[i].state = AgentState::Thinking;
    m.issues[idx].chain_depth = chain_depth;

    let mem = &m.members[i];
    Some(Command::SpawnAgent {
        name: mem.name.clone(),
        issue: issue_id,
        gen: m.team_gen,
        backend: mem.backend,
        model: mem.model.clone(),
        env: m.agent_env.merged_for(mem.backend),
        system_prompt: mem.system_prompt.clone(),
        user_input,
    })
}

/// 成员空闲后,取出它 inbox 里的下一条继续干。
/// 队头那条所属议题若已暂停就先不取 —— 取出来也派不掉,白白在队里打转。
fn drain_inbox(m: &mut Model, name: &str) -> Vec<Command> {
    let Some(i) = m.member_index(name) else { return vec![] };
    if m.members[i].state != AgentState::Idle {
        return vec![];
    }
    // 先清掉所属议题已关闭的条目(它们没有归属了,留着只会堵路)
    let dropped = {
        let before = m.members[i].inbox.len();
        let alive: std::collections::HashSet<u64> = m.issues.iter().map(|x| x.id).collect();
        m.members[i].inbox.retain(|a| alive.contains(&a.issue));
        before - m.members[i].inbox.len()
    };
    if dropped > 0 {
        set_hint(m, format!("{name} 有 {dropped} 条排队任务所属议题已关闭,已丢弃"), 8);
    }

    // 找**第一条能派出去的**,而不是只看队头 ——
    // 队头若压在一个暂停的议题上,后面属于其它活跃议题的活会被永久饿死,
    // 而界面上该成员显示「摸鱼」,一点痕迹都没有。
    let pos = m.members[i].inbox.iter().position(|a| {
        m.issue_index(a.issue)
            .map(|idx| !m.issues[idx].paused)
            .unwrap_or(false)
    });
    let Some(pos) = pos else { return vec![] };
    let next = m.members[i].inbox.remove(pos).expect("pos 刚确认存在");
    let depth = m
        .issue_index(next.issue)
        .map(|idx| m.issues[idx].chain_depth)
        .unwrap_or(0);
    match dispatch(m, next.issue, name, next.text, depth) {
        Some(c) => vec![c],
        None => vec![],
    }
}

/// 把所有成员 inbox 队头能派的活都放出来(Ctrl+P 恢复暂停后用)。
fn drain_all_inboxes(m: &mut Model) -> Vec<Command> {
    let names: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let mut cmds = Vec::new();
    for n in names {
        cmds.extend(drain_inbox(m, &n));
    }
    cmds
}

fn now_ts() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// 生成一个未占用的议题名:议题2、议题3…(默认议题算 1 号)。
/// 生成一个未占用的议题名:议题2、议题3…(默认议题算 1 号)。
pub fn next_issue_name(issues: &[Issue]) -> String {
    let existing: std::collections::HashSet<&str> = issues.iter().map(|i| i.name.as_str()).collect();
    for n in 2.. {
        let candidate = format!("议题{n}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

/// 设置状态提示,持续大约 `secs` 秒(1 tick ≈ 150ms)。
fn set_hint(m: &mut Model, text: impl Into<String>, secs: u64) {
    m.status_hint = Some(text.into());
    m.status_hint_until = m.tick.wrapping_add(secs * 7); // 150ms/tick × 7 ≈ 1050ms
}

/// 处理斜杠命令。均为本地命令,不派活给 agent。
fn handle_slash(m: &mut Model, slash: crate::slash::Slash) -> Vec<Command> {
    use crate::slash::Slash;
    match slash {
        Slash::SwitchTeam { name } => {
            let teams_dir = m.teamfly_dir.join("teams");
            let team_dir = teams_dir.join(&name);
            if !team_dir.is_dir() {
                set_hint(m, format!("找不到团队「{name}」;查 {}", teams_dir.display()), 5);
                return vec![];
            }
            let team = match crate::team::load_team(&team_dir) {
                Ok(t) => t,
                Err(e) => {
                    set_hint(m, format!("加载团队「{name}」失败:{e}"), 8);
                    return vec![];
                }
            };
            let count = team.members.len();
            // 先掐掉在跑的 agent:它们跑的是 bypass 权限,留着会和新团队的同名成员
            // 并发写同一个仓库(旧成员对象马上就被丢掉,再没人能停它们)
            let running = m.working_count();
            if running > 0 {
                m.cancel.cancel();
                m.cancel = tokio_util::sync::CancellationToken::new();
            }
            // 代号 +1:旧团队那些迟到的 AgentDone 一律作废
            m.team_gen += 1;
            m.team_name = team.name;
            m.members = team.members; // 旧成员的 raw/inbox 直接丢
            m.selection = Selection::Chat;
            m.scroll = 0;
            if running > 0 {
                set_hint(
                    m,
                    format!("已切到「{}」团队({count} 人);取消了 {running} 个在跑的 agent", m.team_name),
                    8,
                );
            } else {
                set_hint(m, format!("已切到「{}」团队({count} 人)", m.team_name), 5);
            }
            vec![]
        }
        Slash::Drop { name } => {
            let id = m.cur_issue().id;
            let n = crate::worktree::remove_agent(&m.work_dir, &m.teamfly_dir, id, &name);
            if n == 0 {
                set_hint(m, format!("{name} 在当前议题没有 worktree"), 5);
            } else {
                set_hint(m, format!("已丢弃 {name} 的 {n} 个 worktree 和对应分支"), 8);
            }
            vec![]
        }
        Slash::Unknown { text } => {
            set_hint(m, format!("未知斜杠命令:{text}(试试 /team <名> / /drop <名>)"), 5);
            vec![]
        }
    }
}

/// 点击顶部 tab 栏:命中某个议题 tab → 切;命中 [+ 新议题] → 建;命中其它 → 静默。
fn handle_tab_click(m: &mut Model, col: u16) {
    // tab 内容起始列(与 draw_tabs 一致:SIDEBAR_W=20,内容从 area.x 起,即列 20)
    const START: u16 = 20;
    if col < START {
        return;
    }
    // 依样画:窗口逻辑必须和 draw_tabs 一致
    let total = m.issues.len();
    let (start, end, has_prefix, has_suffix) = if total <= 6 {
        (0usize, total, false, false)
    } else {
        let cur = m.current_issue;
        let s = cur.saturating_sub(2);
        let e = (cur + 3).min(total);
        (s, e, s > 0, e < total)
    };

    let mut x = START;
    if has_prefix {
        x = x.saturating_add(display_width("« ") as u16);
    }
    // 每个 tab:格式 " #<name>[ ⚙N][ ⏸] " + " "(spans 里两段之间的空格)
    let working = m.working_count();
    for i in start..end {
        let issue = &m.issues[i];
        let badge = if i == m.current_issue && working > 0 {
            format!(" ⚙{working}")
        } else {
            String::new()
        };
        let paused = if issue.paused { " ⏸" } else { "" };
        let label = format!(" #{}{}{} ", issue.name, badge, paused);
        let w = display_width(&label) as u16;
        if col >= x && col < x + w {
            // 命中 → 切议题
            m.current_issue = i;
            m.selection = Selection::Chat;
            m.scroll = 0;
            set_hint(m, format!("切到议题 {}", i + 1), 3);
            return;
        }
        x = x.saturating_add(w + 1); // +1 是 spans 间的空格
    }
    if has_suffix {
        x = x.saturating_add(display_width(" »") as u16);
    }
    // [+ 新议题]
    let plus_w = display_width("[+ 新议题]") as u16;
    if col >= x && col < x + plus_w {
        // 新建
        let name = next_issue_name(&m.issues);
        m.issues.push(Issue::new(name.clone()));
        m.current_issue = m.issues.len() - 1;
        m.selection = Selection::Chat;
        m.scroll = 0;
        set_hint(m, format!("已建议题:{name}"), 5);
    }
}

/// 显示宽度。用 unicode-width,和终端/ratatui 的排版一致 ——
/// 以前是「码点 > 0x1100 就算 2 列」的启发式,`⚙`/`①`/`…`/变体选择符 全都算错,
/// 导致 tab 点击热区整体偏移,点第 3 个 tab 会切到第 2 个。
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// 议题名里不允许出现的字符。
///
/// 议题名同时当 jsonl 文件名用,所以要挡住所有平台的路径分隔与保留字符:
/// `/ \` 分隔符、`.` 会被当扩展名、Windows 的 `: * ? " < > |`、以及全部控制字符。
/// 漏掉的话每次 `PersistChat` 都会失败,而用户只看到状态行闪一下「落盘失败」,
/// 这个议题的历史一条都没存下来。
fn is_bad_name_char(c: char) -> bool {
    matches!(c, '/' | '\\' | '.' | '@' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        || c.is_control()
}

/// Windows 保留设备名(不分大小写)。用它当文件名在 Windows 上必然失败。
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 从我的第一句话里派生议题名(前 20 字符,清掉不能进文件名的字符,
/// 若和已有议题重名则附 -2/-3…)。当前议题的原名允许重复(是它自己)。
fn derive_issue_name(first_msg: &str, issues: &[Issue], current: usize) -> String {
    let s: String = first_msg
        .chars()
        .filter(|c| !is_bad_name_char(*c))
        .take(20)
        .collect();
    let s = s.trim().to_string();
    let mut base = if s.is_empty() { "新议题".to_string() } else { s };
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(&base)) {
        base.push('_'); // CON → CON_
    }

    // 去重。**按大小写不敏感比**:macOS/Windows 的文件系统不区分大小写,
    // 「Fix login」和「fix login」会写进同一个 jsonl,互相覆盖、关一个删两个。
    let taken: Vec<String> = issues
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != current)
        .map(|(_, x)| x.name.to_lowercase())
        .collect();
    let clash = |cand: &str| taken.iter().any(|t| t == &cand.to_lowercase());
    if !clash(&base) {
        return base;
    }
    for n in 2.. {
        let cand = format!("{base}-{n}");
        if !clash(&cand) {
            return cand;
        }
    }
    unreachable!()
}

/// pending_delete 的确认窗口(tick 数,一 tick ≈ 150ms,33 tick ≈ 5s)。
const DELETE_CONFIRM_TICKS: u64 = 33;

/// Ctrl+W:关闭当前议题。空议题一键关;有内容先提示确认,窗口期内再按才真删。
fn handle_close_issue(m: &mut Model) -> Vec<Command> {
    if m.issues.len() <= 1 {
        set_hint(m, "至少要留一个议题,不能关", 5);
        return vec![];
    }
    let idx = m.current_issue;
    let has_content = !m.issues[idx].timeline.is_empty();

    if has_content {
        // 二次确认逻辑
        match m.pending_delete {
            Some((pidx, t0)) if pidx == idx && m.tick.wrapping_sub(t0) < DELETE_CONFIRM_TICKS => {
                // 窗口期内再按,真删
                m.pending_delete = None;
            }
            _ => {
                // 首次或超窗:登记 pending,提示
                let n = m.issues[idx].timeline.len();
                m.pending_delete = Some((idx, m.tick));
                let hint = format!(
                    "议题「{}」有 {n} 条消息;再按 ^W 确认删除(5s 内)",
                    m.issues[idx].name
                );
                set_hint(m, hint, 5);
                return vec![];
            }
        }
    }

    // 真删:从 Model 移除 + 删 jsonl
    let removed = m.issues.remove(idx);
    m.pending_delete = None;
    // 调整 current_issue
    if m.current_issue >= m.issues.len() {
        m.current_issue = m.issues.len() - 1;
    }
    m.selection = Selection::Chat;
    m.scroll = 0;
    // 清掉所有成员里该议题的 last_seen 条目,免得 HashMap 随议题数无限膨胀
    for mem in &mut m.members {
        mem.last_seen.remove(&removed.id);
    }
    // 关议题时只清掉**这个议题**的 worktree。以前是删全部,
    // 会把别的议题里用户还没采纳的改动一起干掉。
    crate::worktree::remove_issue(&m.work_dir, &m.teamfly_dir, removed.id);
    set_hint(m, format!("已关闭议题:{}", removed.name), 5);
    vec![Command::DeleteIssueFile { issue: removed.name }]
}

/// 根据当前输入,算出 @ 补全建议(正在输入的最后一个 @token)。
/// 返回匹配的成员名列表;无 @ 或已是完整名则为空。
pub fn at_suggestions(input: &str, roster: &[String]) -> Vec<String> {
    // 找最后一个 '@' 之后的片段,且该片段里不含空格(还在输入这个名字)
    let Some(at) = input.rfind('@') else { return vec![] };
    let frag = &input[at + '@'.len_utf8()..];
    if frag.contains(char::is_whitespace) {
        return vec![]; // @后已有空格,认为这个 @ 已输入完毕
    }
    // 已经精确等于某个名字 → 不再提示(避免刷屏)
    if roster.iter().any(|n| n == frag) {
        return vec![];
    }
    roster
        .iter()
        .filter(|n| frag.is_empty() || n.starts_with(frag))
        .cloned()
        .collect()
}

// ---- runtime:执行 Command ----

fn execute(tx: &UnboundedSender<Msg>, model: &Model, cmd: Command) {
    match cmd {
        Command::PersistChat { issue, msg } => {
            let dir = model.teamfly_dir.clone();
            if let Err(e) = crate::issue::append_chat(&dir, &issue, &msg) {
                let _ = tx.send(Msg::IoError { detail: format!("{e:#}") });
            }
        }
        Command::DeleteIssueFile { issue } => {
            let dir = model.teamfly_dir.clone();
            if let Err(e) = crate::issue::delete_file(&dir, &issue) {
                let _ = tx.send(Msg::IoError { detail: format!("{e:#}") });
            }
        }
        Command::RenameIssueFile { from, to } => {
            let dir = model.teamfly_dir.clone();
            if let Err(e) = crate::issue::rename_file(&dir, &from, &to) {
                let _ = tx.send(Msg::IoError { detail: format!("{e:#}") });
            }
        }
        Command::SpawnAgent {
            name,
            issue,
            gen,
            backend,
            model: mdl,
            env,
            system_prompt,
            user_input,
        } => {
            // 每个 agent 每个议题一个隔离 worktree + 分支，互不冲突。
            // 非 git 仓库时 fallback 到共用 work_dir（和以前一样）。
            let wt = crate::worktree::prepare(
                &model.work_dir,
                &model.teamfly_dir,
                issue,
                &name,
            );
            let mcp_config = resolve_mcp_config(&model.work_dir);
            let spec = RunSpec {
                name,
                issue,
                gen,
                backend,
                model: mdl,
                env,
                system_prompt,
                user_input,
                worktree: wt.branch.clone().map(|b| (wt.agent_dir.clone(), b)),
                work_dir: wt.agent_dir,
                mcp_config,
            };
            let tx = tx.clone();
            let cancel = model.cancel.clone();
            tokio::spawn(async move {
                backend::run(spec, cancel, tx).await;
            });
        }
    }
}

// ---- 主循环 ----

pub async fn run(model: Model) -> Result<()> {
    // panic 兜底:raw mode + 备用屏下 panic 会把终端留在无回显、无光标的状态,
    // 而 panic 信息打在备用屏上随退出一起被抹掉 —— 现象是「终端瞬间花掉且没有报错」。
    // 装个 hook 先把终端恢复,再让默认 hook 正常打印。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_hook(info);
    }));

    // 终端初始化
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx) = unbounded_channel::<Msg>();
    let rt = Runtime::new(tx.clone());
    // model 会被移进 run_loop,先把需要的东西取出来
    let cancel = model.cancel.clone();
    let lock_path = model.teamfly_dir.join("teamfly.lock");

    let res = run_loop(&mut terminal, model, rt, tx, rx).await;

    // 退出前掐掉所有在跑的 agent。不然它们会变成孤儿,继续用 bypass 权限改工作区,
    // 而用户已经看不到任何界面了。
    cancel.cancel();
    // 清掉实例锁(崩溃/panic 时 lock 留着也没事,下次启动会检测到 pid 已死并覆盖)
    let _ = std::fs::remove_file(lock_path);

    // 清理:一律尽力而为,不用 ? 提前 return ——
    // 一旦中途 return,后面的 LeaveAlternateScreen/show_cursor 就不会执行,
    // 用户会被留在无回显的备用屏里,只能盲敲 reset。
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut model: Model,
    rt: Runtime,
    tx: UnboundedSender<Msg>,
    mut rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    // 键盘/鼠标事件流
    let mut events = EventStream::new();
    // spinner tick
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(150));
        loop {
            interval.tick().await;
            if tick_tx.send(Msg::Tick).is_err() {
                break;
            }
        }
    });

    // 首帧
    let mut draw_info = tui::DrawInfo::default();
    terminal.draw(|f| tui::draw(f, &model, &mut draw_info))?;

    loop {
        tokio::select! {
            maybe_ev = events.next() => {
                if let Some(Ok(ev)) = maybe_ev {
                    if let Some(msg) = translate_event(ev, &model) {
                        let cmds = update(&mut model, msg);
                        for c in cmds { rt.exec(&model, c); }
                    }
                }
            }
            maybe_msg = rx.recv() => {
                if let Some(msg) = maybe_msg {
                    let cmds = update(&mut model, msg);
                    for c in cmds { rt.exec(&model, c); }
                }
            }
        }

        if model.should_quit {
            break;
        }
        terminal.draw(|f| tui::draw(f, &model, &mut draw_info))?;
        model.scroll = model.scroll.min(draw_info.scroll_max);
    }
    Ok(())
}

/// 把 crossterm 事件翻译成 Msg。
fn translate_event(ev: Event, model: &Model) -> Option<Msg> {
    match ev {
        Event::Key(k) => Some(Msg::Key(k)),
        Event::Mouse(me) => {
            match me.kind {
                // 滚轮:只在右侧时间线区域生效,其它位置/视图一律吞掉
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    if model.selection == Selection::Chat && me.column >= 20 {
                        let code = if matches!(me.kind, MouseEventKind::ScrollUp) {
                            KeyCode::PageUp
                        } else {
                            KeyCode::PageDown
                        };
                        return Some(Msg::Key(crossterm::event::KeyEvent::new(
                            code,
                            KeyModifiers::empty(),
                        )));
                    }
                    return None; // 明确吞掉,不让任何滚轮事件穿透到下面的 click 处理
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // tab 栏在 y=1(顶部品牌+tab 栏共 3 行,其中中间那行是内容)
                    if me.row == 1 {
                        return Some(Msg::MouseTabClick { col: me.column });
                    }
                    // 左栏点击(品牌角占 3 行,左栏内容从 y=3 起)
                    const BODY_TOP: u16 = 3;
                    if me.column < 20 && me.row >= BODY_TOP {
                        let rel = me.row - BODY_TOP;
                        if rel == 2 {
                            return Some(Msg::Select(Selection::Chat));
                        } else if rel >= 4 {
                            let idx = ((rel - 4) / 2) as usize;
                            if idx < model.members.len() {
                                return Some(Msg::Select(Selection::Member(idx)));
                            }
                        }
                    }
                }
                _ => {}
            }
            None
        }
        _ => None,
    }
}

/// 给别的模块的测试用的最小 Model 构造(仅测试编译时存在)。
#[cfg(test)]
pub mod test_support {
    use super::*;

    pub fn tiny_member(name: &str) -> Member {
        Member {
            name: name.into(),
            role: "角色".into(),
            emoji: "👤".into(),
            backend: BackendKind::Claude,
            model: None,
            system_prompt: String::new(),
            state: AgentState::Working,
            inbox: std::collections::VecDeque::new(),
            raw: std::collections::VecDeque::new(),
            last_seen: std::collections::HashMap::new(),
        }
    }

    pub fn tiny_model() -> Model {
        Model {
            team_name: "T".into(),
            work_dir: std::env::temp_dir(),
            teamfly_dir: std::env::temp_dir().join(".af_tiny"),
            agent_env: crate::env::AgentEnv::default(),
            members: vec![],
            issues: vec![Issue::new("i")],
            current_issue: 0,
            selection: Selection::Chat,
            input: String::new(),
            scroll: 0,
            tick: 0,
            should_quit: false,
            max_chain_depth: 12,
            status_hint: None,
            status_hint_until: 0,
            pending_delete: None,
            show_help: false,
            cancel: tokio_util::sync::CancellationToken::new(),
            team_gen: 0,
        }
    }
}

// ---- 无终端的端到端测试 ----

#[cfg(test)]
mod e2e {
    use super::*;
    use crate::model::{AgentState, BackendKind, Issue, Member, Selection};
    use std::collections::VecDeque;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(m: &mut Model, code: KeyCode) {
        let _ = update(m, Msg::Key(KeyEvent::new(code, KeyModifiers::empty())));
    }
    fn ctrl(m: &mut Model, c: char) {
        let _ = update(m, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)));
    }
    fn key_cmds(m: &mut Model, code: KeyCode) -> Vec<Command> {
        update(m, Msg::Key(KeyEvent::new(code, KeyModifiers::empty())))
    }
    fn ctrl_cmds(m: &mut Model, c: char) -> Vec<Command> {
        update(m, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)))
    }
    fn done(m: &mut Model, name: &str, issue: u64, output: &str) -> Vec<Command> {
        update(
            m,
            Msg::AgentDone {
                name: name.into(),
                issue,
                gen: 0,
                worktree: None,
                full_output: output.into(),
                ok: true,
                err: None,
            },
        )
    }

    fn min_model() -> Model {
        Model {
            team_name: "T".into(),
            work_dir: std::env::temp_dir(),
            teamfly_dir: std::env::temp_dir().join(".af_x"),
            agent_env: crate::env::AgentEnv::default(),
            members: vec![
                member("老K", BackendKind::Claude, "架构"),
                member("阿码", BackendKind::Claude, "实现"),
                member("阿测", BackendKind::Claude, "测试"),
            ],
            issues: vec![Issue::new("i")],
            current_issue: 0,
            selection: Selection::Chat,
            input: String::new(),
            scroll: 0,
            tick: 0,
            should_quit: false,
            max_chain_depth: 12,
            status_hint: None,
            status_hint_until: 0,
            pending_delete: None,
            show_help: false,
            cancel: tokio_util::sync::CancellationToken::new(),
            team_gen: 0,
        }
    }

    #[test]
    fn input_typing_and_backspace() {
        let mut m = min_model();
        for c in "abc".chars() { key(&mut m, KeyCode::Char(c)); }
        assert_eq!(m.input, "abc");
        key(&mut m, KeyCode::Backspace);
        assert_eq!(m.input, "ab");
        // DEL(0x7f) 也应删除
        key(&mut m, KeyCode::Char('\u{7f}'));
        assert_eq!(m.input, "a");
        // BS(0x08) 也应删除
        key(&mut m, KeyCode::Char('\u{8}'));
        assert_eq!(m.input, "");
        // Ctrl+U 清空(Ctrl+H 已让位给帮助的传统语义,不再删字符)
        for c in "xy".chars() { key(&mut m, KeyCode::Char(c)); }
        ctrl(&mut m, 'u');
        assert_eq!(m.input, "");
    }

    #[test]
    fn at_suggestion_and_tab_complete() {
        let mut m = min_model();
        for c in "@阿".chars() { key(&mut m, KeyCode::Char(c)); }
        let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
        let s = at_suggestions(&m.input, &roster);
        assert_eq!(s, vec!["阿码".to_string(), "阿测".to_string()]);
        // Tab 补全成第一个
        key(&mut m, KeyCode::Tab);
        assert_eq!(m.input, "@阿码 ");
        // 补全后不再提示
        assert!(at_suggestions(&m.input, &roster).is_empty());
    }

    #[test]
    fn at_suggestion_empty_frag_lists_all() {
        let m = min_model();
        let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
        assert_eq!(at_suggestions("干活 @", &roster).len(), 3);
        // @后有空格 = 已输完,不提示
        assert!(at_suggestions("@老K 继续", &roster).is_empty());
        // 无 @ 不提示
        assert!(at_suggestions("普通留言", &roster).is_empty());
    }

    #[test]
    fn ctrl_n_creates_and_switches_immediately() {
        let mut m = min_model();
        assert_eq!(m.issues.len(), 1);
        ctrl(&mut m, 'n');
        assert_eq!(m.issues.len(), 2);
        assert_eq!(m.issues[1].name, "议题2");
        assert_eq!(m.current_issue, 1);
        assert!(m.input.is_empty());
    }

    #[test]
    fn ctrl_n_repeated_generates_unique_names() {
        let mut m = min_model();
        ctrl(&mut m, 'n');
        ctrl(&mut m, 'n');
        ctrl(&mut m, 'n');
        let names: Vec<&str> = m.issues.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["i", "议题2", "议题3", "议题4"]);
        assert_eq!(m.current_issue, 3);
    }

    #[test]
    fn next_issue_name_skips_taken() {
        let mut m = min_model();
        // 手动占用 议题2,再按 Ctrl+N 应给出 议题3
        m.issues.push(Issue::new("议题2"));
        ctrl(&mut m, 'n');
        assert_eq!(m.issues.last().unwrap().name, "议题3");
    }

    #[test]
    fn ctrl_w_refuses_when_only_one() {
        let mut m = min_model();
        assert_eq!(m.issues.len(), 1);
        ctrl(&mut m, 'w');
        assert_eq!(m.issues.len(), 1); // 拒绝
        assert!(m.status_hint.as_ref().unwrap().contains("至少"));
    }

    #[test]
    fn ctrl_w_closes_empty_issue_immediately() {
        let mut m = min_model();
        ctrl(&mut m, 'n'); // 建 议题2(空)
        assert_eq!(m.issues.len(), 2);
        assert_eq!(m.current_issue, 1);
        ctrl(&mut m, 'w'); // 空议题直接关
        assert_eq!(m.issues.len(), 1);
        assert_eq!(m.current_issue, 0);
    }

    #[test]
    fn ctrl_w_needs_double_press_when_content() {
        let mut m = min_model();
        ctrl(&mut m, 'n'); // 建议题2
        // 塞一条消息
        m.issues[1].timeline.push(ChatMsg {
            ts: "t".into(), author: "我".into(), text: "x".into(), is_system: false,
        });
        // 第一次:提示确认
        let cmds = update(&mut m, Msg::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
        assert!(cmds.is_empty());
        assert_eq!(m.issues.len(), 2); // 未删
        assert!(m.pending_delete.is_some());
        assert!(m.status_hint.as_ref().unwrap().contains("再按"));
        // 第二次:真删,返回 DeleteIssueFile Command
        let cmds = update(&mut m, Msg::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
        assert_eq!(m.issues.len(), 1);
        assert!(matches!(cmds.first(), Some(Command::DeleteIssueFile { .. })));
    }

    #[test]
    fn ctrl_w_pending_expires() {
        let mut m = min_model();
        ctrl(&mut m, 'n');
        m.issues[1].timeline.push(ChatMsg {
            ts: "t".into(), author: "我".into(), text: "x".into(), is_system: false,
        });
        ctrl(&mut m, 'w'); // 首次 → pending
        // 时间流逝
        m.tick = m.tick.wrapping_add(100);
        ctrl(&mut m, 'w'); // 超窗,视为再次首次,又只提示
        assert_eq!(m.issues.len(), 2); // 未删
        assert!(m.pending_delete.is_some());
    }

    // ---- 回归:issue 绑定 / @ 截断 / paused 入队 / Ctrl+C 取消 ----

    #[test]
    fn report_goes_to_its_own_issue_after_tab_switch() {
        let mut m = min_model();
        let issue_a = m.issues[0].id;
        m.members[0].state = AgentState::Working;
        // 干活期间用户按 ^N 切到新议题
        ctrl(&mut m, 'n');
        assert_eq!(m.current_issue, 1);

        done(&mut m, "老K", issue_a, "A 议题干完了");

        // 汇报落在它自己的议题,不是当前选中的那个
        assert_eq!(m.issues[0].timeline.len(), 1);
        assert_eq!(m.issues[0].timeline[0].text, "A 议题干完了");
        assert!(m.issues[1].timeline.is_empty());
        // 连锁深度也必须加在原议题上,否则原议题的防乒乓永远不触发
        assert_eq!(m.issues[0].chain_depth, 1);
        assert_eq!(m.issues[1].chain_depth, 0);
    }

    #[test]
    fn report_of_closed_issue_is_dropped_not_misfiled() {
        let mut m = min_model();
        ctrl(&mut m, 'n'); // 建 议题2 并切过去
        let gone = m.issues[1].id;
        m.members[0].state = AgentState::Working;
        ctrl(&mut m, 'w'); // 空议题一键关
        assert_eq!(m.issues.len(), 1);

        let cmds = done(&mut m, "老K", gone, "已关议题的汇报");

        // 绝不能写进剩下那个无关议题
        assert!(m.issues[0].timeline.is_empty());
        assert!(!cmds.iter().any(|c| matches!(c, Command::PersistChat { .. })));
        assert_eq!(m.members[0].state, AgentState::Idle);
    }

    #[test]
    fn report_mentions_survive_truncation() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.members[0].state = AgentState::Working;
        // 800 字汇报,按团队规约把 @ 写在结尾
        let long = "改完了。".repeat(200);
        let cmds = done(&mut m, "老K", id, &format!("{long}\n@阿码 接着上单测"));

        // 接力没断
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::SpawnAgent { name, .. } if name == "阿码")));
        assert_eq!(m.members[1].state, AgentState::Thinking);
        // 群聊里是截断版,且标注了派给谁
        let chat = &m.issues[0].timeline[0].text;
        assert!(chat.contains('…'));
        assert!(chat.contains("↪ 已派给 @阿码"));
    }

    #[test]
    fn paused_issue_queues_assignment_and_ctrl_p_releases_it() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.issues[0].paused = true;
        m.members[0].state = AgentState::Working;

        let cmds = done(&mut m, "老K", id, "看完了 @阿码 接手");

        // 暂停中不起进程,但活必须进队列 —— 以前这里直接丢了
        assert!(!cmds.iter().any(|c| matches!(c, Command::SpawnAgent { .. })));
        assert_eq!(m.members[1].inbox.len(), 1);
        assert_eq!(m.members[1].inbox[0].issue, id);

        // Ctrl+P 恢复 → 排队的活被放出来
        let cmds = ctrl_cmds(&mut m, 'p');
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::SpawnAgent { name, .. } if name == "阿码")));
        assert!(m.members[1].inbox.is_empty());
        assert_eq!(m.members[1].state, AgentState::Thinking);
    }

    #[test]
    fn ctrl_c_cancels_running_agents_before_quitting() {
        let mut m = min_model();
        m.members[0].state = AgentState::Working;
        let old = m.cancel.clone();

        ctrl(&mut m, 'c');
        assert!(!m.should_quit, "有 agent 在跑时第一次 ^C 应该只取消,不退出");
        assert!(old.is_cancelled());
        // 必须换新 token,否则之后新起的 agent 一生下来就是取消态
        assert!(!m.cancel.is_cancelled());

        // 没有 agent 在跑时才真退出
        m.members[0].state = AgentState::Idle;
        ctrl(&mut m, 'c');
        assert!(m.should_quit);
    }

    #[test]
    fn stale_team_generation_result_is_discarded() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.members[0].state = AgentState::Working;
        // 模拟 /team 热切
        m.team_gen += 1;

        // 旧团队的 agent 迟到交卷(带旧代号)
        let cmds = update(
            &mut m,
            Msg::AgentDone {
                name: "老K".into(),
                issue: id,
                gen: 0,
                worktree: None,
                full_output: "旧团队的汇报 @阿码 接手".into(),
                ok: true,
                err: None,
            },
        );

        // 不入时间线、不落盘,更不能按新花名册把同名的人唤醒
        assert!(m.issues[0].timeline.is_empty());
        assert!(cmds.is_empty());
        assert_eq!(m.members[1].state, AgentState::Idle);
        assert!(m.members[1].inbox.is_empty());
    }

    #[test]
    fn chain_cap_queues_mentions_instead_of_dropping() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.max_chain_depth = 1;
        m.issues[0].chain_depth = 1; // 下一轮就触顶
        m.members[0].state = AgentState::Working;

        let cmds = done(&mut m, "老K", id, "还得继续 @阿码 接手");

        // 触顶暂停了
        assert!(m.issues[0].paused);
        // 但这一轮的 @ 不能蒸发 —— 必须在队里等 ^P
        assert!(!cmds.iter().any(|c| matches!(c, Command::SpawnAgent { .. })));
        assert_eq!(m.members[1].inbox.len(), 1, "触顶时 @ 被丢掉了");

        // ^P 之后真的能放出来(以前必然是「放出 0 条」)
        let cmds = ctrl_cmds(&mut m, 'p');
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::SpawnAgent { name, .. } if name == "阿码")));
    }

    #[test]
    fn ctrl_c_also_clears_queued_work() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.members[0].state = AgentState::Working;
        m.members[1].inbox.push_back(Assignment { issue: id, text: "排队的活".into() });

        ctrl(&mut m, 'c');
        // 不清队列的话,被取消的成员一交卷就走 drain_inbox,拿新 token 立刻重起,
        // 用户既停不下来也退不出去
        assert!(m.members[1].inbox.is_empty());
        assert!(m.status_hint.as_ref().unwrap().contains("清掉 1 条"));

        // 交卷后确实不会再起进程
        let cmds = update(&mut m, Msg::AgentDone {
            name: "老K".into(), issue: id, gen: 0, worktree: None,
            full_output: String::new(), ok: false,
            err: Some(crate::model::CANCELLED.into()),
        });
        assert!(!cmds.iter().any(|c| matches!(c, Command::SpawnAgent { .. })));
    }

    #[test]
    fn paused_head_does_not_starve_other_issues() {
        let mut m = min_model();
        let a = m.issues[0].id;
        ctrl(&mut m, 'n'); // 建议题2
        let b = m.issues[1].id;
        // 队头压在暂停的议题A 上,后面是活跃议题B 的活
        m.issues[0].paused = true;
        m.members[0].inbox.push_back(Assignment { issue: a, text: "A 的活".into() });
        m.members[0].inbox.push_back(Assignment { issue: b, text: "B 的活".into() });

        // 成员空闲后交卷触发 drain
        let cmds = update(&mut m, Msg::AgentDone {
            name: "老K".into(), issue: b, gen: 0, worktree: None,
            full_output: "干完了".into(), ok: true, err: None,
        });

        // B 的活必须被放出来,不能被 A 的队头永久堵死
        assert!(
            cmds.iter().any(|c| matches!(c, Command::SpawnAgent { issue, .. } if *issue == b)),
            "队头压在暂停议题上,把后面别的议题的活饿死了"
        );
        // A 的活还留在队里等 ^P
        assert_eq!(m.members[0].inbox.len(), 1);
        assert_eq!(m.members[0].inbox[0].issue, a);
    }

    #[test]
    fn issue_name_rejects_filesystem_hostile_chars() {
        // 这些字符进了文件名,每条 PersistChat 都会失败,整个议题的历史一条都存不下来
        let issues = vec![Issue::new("别的")];
        for msg in [
            "@DEV 修 config:prod 的 \"bug\"?",
            "改 a/b\\c.rs",
            "带\t制表符和\u{7}响铃",
            "通配 * 和 ? 还有 <> |",
        ] {
            let name = derive_issue_name(msg, &issues, 0);
            assert!(
                !name.chars().any(is_bad_name_char),
                "{msg:?} 派生出的名字 {name:?} 仍含非法字符"
            );
        }
        // Windows 保留设备名要避开
        assert_eq!(derive_issue_name("CON", &issues, 0), "CON_");
        assert_eq!(derive_issue_name("nul", &issues, 0), "nul_");
    }

    #[test]
    fn issue_name_dedup_is_case_insensitive() {
        // macOS/Windows 文件系统不区分大小写:两个只差大小写的议题会写进同一个 jsonl,
        // 互相覆盖,关一个删两个
        let issues = vec![Issue::new("Fix login"), Issue::new("别的")];
        let name = derive_issue_name("fix login", &issues, 1);
        assert_ne!(name.to_lowercase(), "fix login", "只差大小写也算撞名");
        assert_eq!(name, "fix login-2");
    }

    #[test]
    fn auto_rename_keeps_persisted_file() {
        let mut m = min_model();
        // 议题名是自动生成的那种,且已经落过一条系统消息
        m.issues[0].name = "议题2".into();
        m.issues[0].timeline.push(ChatMsg {
            ts: "t".into(), author: "系统".into(), text: "老K 掉线:挂了".into(), is_system: true,
        });
        for c in "开始正式干活".chars() { key(&mut m, KeyCode::Char(c)); }
        let cmds = key_cmds(&mut m, KeyCode::Enter);
        // 必须是改名而不是删掉 —— 删掉会把已落盘的掉线记录永久丢掉
        assert!(
            cmds.iter().any(|c| matches!(c, Command::RenameIssueFile { from, to }
                if from == "议题2" && to == "开始正式干活")),
            "自动改名应发 RenameIssueFile,实际是 {cmds:?}"
        );
        assert!(!cmds.iter().any(|c| matches!(c, Command::DeleteIssueFile { .. })));
    }

    #[test]
    fn alt_plus_letter_does_not_leak_into_input() {
        let mut m = min_model();
        let _ = update(&mut m, Msg::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)));
        assert!(m.input.is_empty(), "Alt+a 不该被当成普通输入插进输入框");
    }

    #[test]
    fn inbox_has_a_cap() {
        let mut m = min_model();
        let id = m.issues[0].id;
        m.members[1].state = AgentState::Working; // 忙 → 都进队列
        for i in 0..(crate::model::INBOX_CAP + 5) {
            dispatch(&mut m, id, "阿码", format!("活{i}"), 0);
        }
        assert_eq!(m.members[1].inbox.len(), crate::model::INBOX_CAP);
        // 丢活必须有提示
        assert!(m.status_hint.as_ref().unwrap().contains("待办已满"));
    }

    #[test]
    fn handoff_tells_downstream_where_upstream_changes_are() {
        // 没有 worktree(fallback 模式):就是普通指派
        let plain = handoff_text("DEV", "改完了 @REV", None);
        assert_eq!(plain, "[来自 DEV] 改完了 @REV");
        assert!(!plain.contains("git merge"));

        // 有 worktree:必须明确给出分支名和用法 —— 接力者在自己的 worktree 里,
        // 看到的是旧代码,不说它就只能瞎猜
        let with_wt = handoff_text("DEV", "改完了 @REV", Some("teamfly/DEV/abc1234"));
        assert!(with_wt.contains("teamfly/DEV/abc1234"));
        assert!(with_wt.contains("git merge teamfly/DEV/abc1234"));
        assert!(with_wt.contains("看到的还是改动前的代码"));
        // 原汇报内容不能丢
        assert!(with_wt.contains("改完了 @REV"));
    }

    #[test]
    fn io_error_surfaces_as_hint() {
        let mut m = min_model();
        update(&mut m, Msg::IoError { detail: "磁盘已满".into() });
        assert!(m.status_hint.as_ref().unwrap().contains("落盘失败"));
        assert!(m.status_hint.as_ref().unwrap().contains("磁盘已满"));
    }

    fn member(name: &str, backend: BackendKind, role: &str) -> Member {
        Member {
            name: name.into(),
            role: role.into(),
            emoji: "👤".into(),
            backend,
            model: None,
            system_prompt: if role == "架构" { "架构".into() } else { String::new() },
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            raw: VecDeque::new(),
            last_seen: std::collections::HashMap::new(),
        }
    }

}
