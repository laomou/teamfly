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
    m.status_hint = None;

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
                m.status_hint = Some("已恢复".into());
                return vec![];
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap() as usize;
                if n >= 1 && n <= m.issues.len() {
                    m.current_issue = n - 1;
                    m.selection = Selection::Chat;
                    m.scroll = 0;
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

    let mut cmds = Vec::new();
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
        mcp_config: mem.mcp_config.clone(),
        env: m.agent_env.clone(),
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
        Command::SpawnAgent {
            name,
            backend,
            model: mdl,
            mcp_config,
            env,
            system_prompt,
            user_input,
        } => {
            let spec = RunSpec {
                name,
                backend,
                model: mdl,
                mcp_config,
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
                // 左栏点击:品牌角占 3 行,左栏内容从 y=3 起。
                // 左栏内部行序:0=团队名 1=空 2=#群聊 3=空 4..成员(每人 2 行)
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
            agent_env: std::collections::HashMap::new(),
            members: vec![
                member("老K", BackendKind::Mock, "架构"),
                member("阿码", BackendKind::Mock, "实现"),
                member("阿测", BackendKind::Mock, "测试"),
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
        // Ctrl+H 删除
        for c in "xy".chars() { key(&mut m, KeyCode::Char(c)); }
        ctrl(&mut m, 'h');
        assert_eq!(m.input, "x");
        // Ctrl+U 清空
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

    fn member(name: &str, backend: BackendKind, role: &str) -> Member {
        Member {
            name: name.into(),
            role: role.into(),
            emoji: "👤".into(),
            backend,
            model: None,
            mcp_config: None,
            system_prompt: if role == "架构" { "架构".into() } else { String::new() },
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            raw: VecDeque::new(),
            last_seen_chat_len: 0,
        }
    }

    fn model(dir: &std::path::Path) -> Model {
        Model {
            team_name: "演示队".into(),
            work_dir: dir.to_path_buf(),
            teamfly_dir: dir.join(".teamfly"),
            agent_env: std::collections::HashMap::new(),
            members: vec![
                member("老K", BackendKind::Mock, "架构"),
                member("阿码", BackendKind::Mock, "实现"),
                member("阿测", BackendKind::Mock, "测试"),
            ],
            issues: vec![Issue::new("重构登录")],
            current_issue: 0,
            selection: Selection::Chat,
            input: String::new(),
            scroll: 0,
            tick: 0,
            should_quit: false,
            max_chain_depth: 12,
            status_hint: None,
        }
    }

    /// 驱动整个 TEA 循环直到全体空闲。返回最终 Model。
    async fn drive(mut m: Model, first_input: &str) -> Model {
        let (tx, mut rx) = unbounded_channel::<Msg>();
        std::fs::create_dir_all(&m.teamfly_dir).unwrap();
        let rt = Runtime::new(tx.clone());

        // 我输入
        m.input = first_input.to_string();
        let cmds = update(&mut m, Msg::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        )));
        for c in cmds { rt.exec(&m, c); }

        // 处理消息直到通道空闲一段时间
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(800), rx.recv()).await {
                Ok(Some(msg)) => {
                    let cmds = update(&mut m, msg);
                    for c in cmds { rt.exec(&m, c); }
                }
                _ => break, // 超时 = 没有更多消息,收敛
            }
        }
        m
    }

    #[tokio::test]
    async fn full_flow_owner_to_agents() {
        let tmp = std::env::temp_dir().join(format!("teamfly_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let m = model(&tmp);
        // 我 @老K → 老K(mock,架构)会 @阿码 → 阿码 @阿测
        let m = drive(m, "@老K 重构一下登录模块").await;

        let tl = &m.issues[0].timeline;
        let authors: Vec<&str> = tl.iter().map(|x| x.author.as_str()).collect();
        // 应包含:我、老K、阿码、阿测 的汇报
        assert!(authors.contains(&"我"), "authors={:?}", authors);
        assert!(authors.contains(&"老K"), "authors={:?}", authors);
        assert!(authors.contains(&"阿码"), "authors={:?}", authors);
        assert!(authors.contains(&"阿测"), "authors={:?}", authors);
        // 全体最终空闲
        assert!(m.members.iter().all(|x| x.state == AgentState::Idle));
        // 落盘文件存在且非空
        let f = tmp.join(".teamfly/issues/重构登录.jsonl");
        let content = std::fs::read_to_string(&f).unwrap();
        assert!(content.lines().count() >= 4, "jsonl lines: {}", content.lines().count());

        // 重放:load_all_issues 能恢复
        let issues = crate::issue::load_all_issues(&tmp.join(".teamfly")).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].timeline.len(), tl.len());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn no_mention_no_dispatch() {
        let tmp = std::env::temp_dir().join(format!("teamfly_e2e2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let m = model(&tmp);
        let m = drive(m, "只是记个备注,不@任何人").await;
        // 只有我一条,无 agent 被唤醒
        assert_eq!(m.issues[0].timeline.len(), 1);
        assert!(m.members.iter().all(|x| x.state == AgentState::Idle));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
