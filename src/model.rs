//! TEA 核心数据类型：Model / Msg / Command / ChatMsg / 状态机。
//! 纯数据 + 少量纯函数，无 I/O、无 await。

use std::collections::VecDeque;

/// 群友（agent）的运行状态。无「等你」态（已砍拍板）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,     // 💤 摸鱼：空闲，等被 @
    Thinking, // 💭 思考中：刚被唤醒、进程已起但还没产出
    Working,  // ⚙ 干活中：正在读写/跑命令
}

impl AgentState {
    #[allow(dead_code)] // 供未来紧凑视图使用
    pub fn glyph(&self) -> &'static str {
        match self {
            AgentState::Idle => "💤",
            AgentState::Thinking => "💭",
            AgentState::Working => "⚙",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Idle => "摸鱼",
            AgentState::Thinking => "思考中",
            AgentState::Working => "干活中",
        }
    }
}

/// 后端类型：由 frontmatter 静态决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Claude,
    Codex,
    Api,
    Mock, // 测试用：不需要凭证，回显+套路化产出
}

/// 花名册里的一个群友（静态定义 + 运行时状态）。
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub emoji: String,
    pub backend: BackendKind,
    pub model: Option<String>,
    pub mcp_config: Option<String>,
    pub system_prompt: String, // team 公共 + 个人人设 拼装后的最终 prompt
    // 运行时
    pub state: AgentState,
    /// 忙时被 @，进这个待办队列（原文投递内容）
    pub inbox: VecDeque<String>,
    /// raw 输出流（环形缓冲，剥过 ANSI），供单人视图展示
    pub raw: VecDeque<String>,
    /// 该成员上次「活跃」时群聊时间线的长度，用于算增量前情
    pub last_seen_chat_len: usize,
}

pub const RAW_CAP: usize = 2000; // 单 agent raw 环形缓冲上限

impl Member {
    pub fn push_raw(&mut self, line: String) {
        self.raw.push_back(line);
        while self.raw.len() > RAW_CAP {
            self.raw.pop_front();
        }
    }
}

/// 群聊时间线里的一条消息。既是 UI，又是共享上下文。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMsg {
    pub ts: String,            // ISO 时间戳
    pub author: String,        // "我" / agent 名 / "系统"
    pub text: String,          // 展示文本（【群聊】后的内容 或 我原话）
    #[serde(default)]
    pub is_system: bool,       // 掉线/暂停等系统消息
}

/// 一个议题（tab）。属于项目，各有独立时间线。
#[derive(Debug, Clone)]
pub struct Issue {
    pub name: String,
    pub timeline: Vec<ChatMsg>,
    /// 本议题当前「一条我指令引发的 @ 连锁」轮数，用于防乒乓
    pub chain_depth: u32,
    /// 是否因防乒乓被暂停
    pub paused: bool,
}

impl Issue {
    pub fn new(name: impl Into<String>) -> Self {
        Issue {
            name: name.into(),
            timeline: Vec::new(),
            chain_depth: 0,
            paused: false,
        }
    }
}

/// 左栏选中项：群聊 或 某个成员（按索引）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Chat,
    Member(usize),
}

/// 全部应用状态。TEA 的 Model。
pub struct Model {
    pub team_name: String,
    pub work_dir: std::path::PathBuf,
    pub teamfly_dir: std::path::PathBuf,
    /// 全队共享的环境变量,注入到每个 agent 子进程(来自 .teamfly/env.toml)
    pub agent_env: std::collections::HashMap<String, String>,
    pub members: Vec<Member>,
    pub issues: Vec<Issue>,
    pub current_issue: usize,
    pub selection: Selection,
    /// 输入框草稿（入 Model → 抗刷屏）
    pub input: String,
    /// 右区滚动偏移（0 = 贴底）
    pub scroll: u16,
    /// spinner 动画帧
    pub tick: u64,
    /// 是否请求退出
    pub should_quit: bool,
    /// 防乒乓上限
    pub max_chain_depth: u32,
    /// 状态提示行（临时消息，如「已暂停」）
    pub status_hint: Option<String>,
}

impl Model {
    pub fn cur_issue(&self) -> &Issue {
        &self.issues[self.current_issue]
    }
    pub fn cur_issue_mut(&mut self) -> &mut Issue {
        &mut self.issues[self.current_issue]
    }
    pub fn member_index(&self, name: &str) -> Option<usize> {
        self.members.iter().position(|m| m.name == name)
    }
    pub fn working_count(&self) -> usize {
        self.members
            .iter()
            .filter(|m| m.state != AgentState::Idle)
            .count()
    }
}

/// 所有事件汇成的单一消息类型。
#[derive(Debug)]
pub enum Msg {
    /// 键盘输入
    Key(crossterm::event::KeyEvent),
    /// 鼠标选中左栏某项
    Select(Selection),
    /// 某 agent 进程吐了一行 raw（已剥 ANSI）
    AgentStdout { name: String, line: String },
    /// 某 agent 一轮结束
    AgentDone {
        name: String,
        /// 整轮 raw 汇总（用于兜底提取汇报）
        full_output: String,
        ok: bool,
        err: Option<String>,
    },
    /// spinner 定时器
    Tick,
}

/// update 返回的副作用描述，由 runtime 执行后回投 Msg。
#[derive(Debug)]
pub enum Command {
    /// 起一个 agent 进程干活：喂 prompt（含增量前情），流式回 AgentStdout，结束回 AgentDone
    SpawnAgent {
        name: String,
        backend: BackendKind,
        model: Option<String>,
        mcp_config: Option<String>,
        env: std::collections::HashMap<String, String>,
        system_prompt: String,
        user_input: String, // 增量前情 + 本次指派
    },
    /// 把一条群聊消息追加落盘
    PersistChat { issue: String, msg: ChatMsg },
}
