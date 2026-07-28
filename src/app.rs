//! TEA 事件循环 + update + Command 执行。
//! 所有并发事件源汇成单一 mpsc<Msg>,主循环逐条喂 update。

use crate::backend::{self, RunSpec};
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
        Msg::AgentDone { name, full_output, ok, err } => handle_agent_done(m, name, full_output, ok, err),
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
                return vec![];
            }
        }
    }

    // Ctrl 组合
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('c') => {
                m.should_quit = true;
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
                // 恢复暂停
                m.cur_issue_mut().paused = false;
                m.cur_issue_mut().chain_depth = 0;
                set_hint(m, "已恢复", 5);
                return vec![];
            }
            KeyCode::Char('n') => {
                // 直接建一个新议题,名字自动递增,并切过去
                let name = next_issue_name(&m.issues);
                m.issues.push(Issue::new(name.clone()));
                m.current_issue = m.issues.len() - 1;
                m.selection = Selection::Chat;
                m.scroll = 0;
                m.input.clear();
                m.input_mode = InputMode::Chat;
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
            if m.input_mode == InputMode::NewIssue {
                // 取消新建议题
                m.input_mode = InputMode::Chat;
                m.input.clear();
                set_hint(m, "已取消新建议题", 5);
            } else {
                m.selection = Selection::Chat;
                m.scroll = 0;
            }
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

    // 斜杠命令:/init 展开成消息,/team 直接切队
    if m.input_mode == InputMode::Chat {
        if let Some(slash) = crate::slash::parse(&text) {
            return handle_slash(m, slash);
        }
    }

    // 新建议题模式:创建议题并切过去,不进时间线
    if m.input_mode == InputMode::NewIssue {
        m.input_mode = InputMode::Chat;
        // 校验:非空、不重名、名字里不含分隔/路径字符(用于落盘 <名>.jsonl)
        if text.contains('/') || text.contains('\\') || text.contains('.') {
            set_hint(m, format!("议题名不能含 / \\ .:{text}"), 5);
            return vec![];
        }
        if m.issues.iter().any(|i| i.name == text) {
            // 已存在,直接切过去
            if let Some(idx) = m.issues.iter().position(|i| i.name == text) {
                m.current_issue = idx;
                m.selection = Selection::Chat;
                m.scroll = 0;
                set_hint(m, format!("议题「{text}」已存在,切过去"), 5);
            }
            return vec![];
        }
        // 新建
        m.issues.push(Issue::new(text.clone()));
        m.current_issue = m.issues.len() - 1;
        m.selection = Selection::Chat;
        m.scroll = 0;
        set_hint(m, format!("已建议题:{text}"), 5);
        return vec![];
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
            cmds.push(Command::DeleteIssueFile { issue: old_name });
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
    let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let mentions = crate::router::parse_owner_mentions(&text, &roster);
    for name in mentions {
        let assignment = format!("[我] {text}");
        if let Some(c) = dispatch(m, &name, assignment, 0) {
            cmds.push(c);
        }
    }
    cmds
}

/// 一轮结束:提取汇报入群聊、落盘、解析 @ 继续派活、处理排队。
fn handle_agent_done(
    m: &mut Model,
    name: String,
    full_output: String,
    ok: bool,
    err: Option<String>,
) -> Vec<Command> {
    let mut cmds = Vec::new();
    let issue_name = m.cur_issue().name.clone();

    // 该成员回到空闲
    let depth_when_started;
    if let Some(i) = m.member_index(&name) {
        m.members[i].state = AgentState::Idle;
    }
    depth_when_started = m.cur_issue().chain_depth;

    if !ok {
        // 掉线 → 系统消息
        let text = format!("{name} 掉线:{}", err.unwrap_or_else(|| "未知错误".into()));
        let msg = ChatMsg { ts: now_ts(), author: "系统".into(), text, is_system: true };
        m.cur_issue_mut().timeline.push(msg.clone());
        cmds.push(Command::PersistChat { issue: issue_name.clone(), msg });
        // 尝试处理该成员排队的下一条
        cmds.extend(drain_inbox(m, &name));
        return cmds;
    }

    // 提取汇报
    let report = crate::router::extract_report(&full_output);
    let msg = ChatMsg { ts: now_ts(), author: name.clone(), text: report.clone(), is_system: false };
    m.cur_issue_mut().timeline.push(msg.clone());
    cmds.push(Command::PersistChat { issue: issue_name.clone(), msg });

    // 防乒乓:连锁深度 +1
    let new_depth = depth_when_started + 1;
    if new_depth > m.max_chain_depth {
        m.cur_issue_mut().paused = true;
        let text = format!(
            "@ 连锁已达 {new_depth} 轮,自动暂停以防打转。按 Ctrl+P 恢复,或直接发新指令。"
        );
        let smsg = ChatMsg { ts: now_ts(), author: "系统".into(), text, is_system: true };
        m.cur_issue_mut().timeline.push(smsg.clone());
        cmds.push(Command::PersistChat { issue: issue_name, msg: smsg });
        return cmds;
    }
    m.cur_issue_mut().chain_depth = new_depth;

    // 解析汇报里的 @,派给在册的人(忽略自 @)
    let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let mentions = crate::router::parse_mentions(&report, &roster, &name);
    for target in mentions {
        let assignment = format!("[来自 {name}] {report}");
        if let Some(c) = dispatch(m, &target, assignment, new_depth) {
            cmds.push(c);
        }
    }

    // 该成员自己的排队任务
    cmds.extend(drain_inbox(m, &name));
    cmds
}

/// 派活给某成员。忙则入队;闲则起进程。返回 SpawnAgent 命令(如需)。
fn dispatch(m: &mut Model, name: &str, assignment: String, chain_depth: u32) -> Option<Command> {
    let i = m.member_index(name)?;
    if m.cur_issue().paused {
        return None;
    }
    if m.members[i].state != AgentState::Idle {
        // 忙 → 排队
        m.members[i].inbox.push_back(assignment);
        return None;
    }
    // 闲 → 起进程
    let timeline = m.cur_issue().timeline.clone();
    let user_input = issue::build_prompt_input(&timeline, &m.members[i], &assignment);
    m.members[i].last_seen_chat_len = timeline.len();
    m.members[i].state = AgentState::Thinking;
    m.cur_issue_mut().chain_depth = chain_depth;

    let mem = &m.members[i];
    Some(Command::SpawnAgent {
        name: mem.name.clone(),
        backend: mem.backend,
        model: mem.model.clone(),
        env: m.agent_env.merged_for(mem.backend),
        system_prompt: mem.system_prompt.clone(),
        user_input,
    })
}

/// 成员空闲后,取出它 inbox 里的下一条继续干。
fn drain_inbox(m: &mut Model, name: &str) -> Vec<Command> {
    let Some(i) = m.member_index(name) else { return vec![] };
    if m.members[i].state != AgentState::Idle {
        return vec![];
    }
    if let Some(next) = m.members[i].inbox.pop_front() {
        let depth = m.cur_issue().chain_depth;
        if let Some(c) = dispatch(m, name, next, depth) {
            return vec![c];
        }
    }
    vec![]
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
            m.team_name = team.name;
            m.members = team.members; // 旧成员的 raw/inbox 直接丢
            m.selection = Selection::Chat;
            m.scroll = 0;
            set_hint(m, format!("已切到「{}」团队({count} 人)", m.team_name), 5);
            vec![]
        }
        Slash::Unknown { text } => {
            set_hint(m, format!("未知斜杠命令:{text}(试试 /init / /team <名>)"), 5);
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

/// 粗略计算显示宽度(CJK/emoji 记 2,其余 1)。
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) > 0x1100 { 2 } else { 1 })
        .sum()
}

/// 从我的第一句话里派生议题名(前 20 字符,去掉 @、清理换行、避免文件名非法字符,
/// 若和已有议题重名则附 -2/-3…)。当前议题的原名允许重复(是它自己)。
fn derive_issue_name(first_msg: &str, issues: &[Issue], current: usize) -> String {
    let s: String = first_msg
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '.' | '\n' | '\r' | '@'))
        .take(20)
        .collect();
    let s = s.trim().to_string();
    let base = if s.is_empty() { "新议题".to_string() } else { s };

    // 去重
    let taken: std::collections::HashSet<&str> = issues
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != current)
        .map(|(_, x)| x.name.as_str())
        .collect();
    if !taken.contains(base.as_str()) {
        return base;
    }
    for n in 2.. {
        let cand = format!("{base}-{n}");
        if !taken.contains(cand.as_str()) {
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
            let _ = crate::issue::append_chat(&dir, &issue, &msg);
        }
        Command::DeleteIssueFile { issue } => {
            let dir = model.teamfly_dir.clone();
            let _ = crate::issue::delete_file(&dir, &issue);
        }
        Command::SpawnAgent {
            name,
            backend,
            model: mdl,
            env,
            system_prompt,
            user_input,
        } => {
            let spec = RunSpec {
                name,
                backend,
                model: mdl,
                env,
                system_prompt,
                user_input,
                work_dir: model.work_dir.clone(),
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                backend::run(spec, tx).await;
            });
        }
    }
}

// ---- 主循环 ----

pub async fn run(model: Model) -> Result<()> {
    // 终端初始化
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx) = unbounded_channel::<Msg>();
    let rt = Runtime::new(tx.clone());

    let res = run_loop(&mut terminal, model, rt, tx, rx).await;

    // 清理
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
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
    terminal.draw(|f| tui::draw(f, &model))?;

    loop {
        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        if let Some(msg) = translate_event(ev, &model) {
                            let cmds = update(&mut model, msg);
                            for c in cmds { rt.exec(&model, c); }
                        }
                    }
                    Some(Err(_)) | None => {}
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
        terminal.draw(|f| tui::draw(f, &model))?;
    }
    Ok(())
}

/// 把 crossterm 事件翻译成 Msg。
fn translate_event(ev: Event, model: &Model) -> Option<Msg> {
    match ev {
        Event::Key(k) => Some(Msg::Key(k)),
        Event::Mouse(me) => {
            if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                // tab 栏在 y=1(顶部品牌+tab 栏共 3 行,其中中间那行是内容)
                if me.row == 1 {
                    // 简化:tab 行上任何点击都当作「切议题」的鼠标操作:
                    //   点在议题名区域 = 尝试命中(暂只支持整体切下一个/上一个)
                    //   点在 [+ 新议题] = 新建
                    // 精确热区需要跨层信息,这里先把 [+ 新议题] 大致识别为"点得比较靠右"
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
            None
        }
        _ => None,
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
            input_mode: InputMode::Chat,
            input: String::new(),
            scroll: 0,
            tick: 0,
            should_quit: false,
            max_chain_depth: 12,
            status_hint: None,
            status_hint_until: 0,
            pending_delete: None,
            show_help: false,
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
        // 无输入直接建 + 切,input_mode 保持 Chat
        assert_eq!(m.input_mode, InputMode::Chat);
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
            last_seen_chat_len: 0,
        }
    }

}
