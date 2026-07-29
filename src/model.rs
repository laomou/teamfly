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
}

/// 花名册里的一个群友（静态定义 + 运行时状态）。
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub emoji: String,
    pub backend: BackendKind,
    pub model: Option<String>,
    pub system_prompt: String, // team 公共 + 个人人设 拼装后的最终 prompt
    /// 只读成员:在**主工作目录**里干活且不给写权限(claude plan 模式 /
    /// codex read-only sandbox)。适合评审、调度这类不该改文件的角色。
    /// 默认 false = 可写,在议题的 worktree 里干活。
    pub read_only: bool,
    // 运行时
    pub state: AgentState,
    /// 忙时/议题暂停时被 @，进这个待办队列
    pub inbox: VecDeque<Assignment>,
    /// 正在为哪个议题干活（Idle 时无意义）。
    /// 用来判断「同议题内是否已有写手在跑」—— 一个议题共享一个 worktree，
    /// 两个写手同时在里面改文件会互相踩。
    pub working_issue: Option<u64>,
    /// raw 输出流（环形缓冲，剥过 ANSI），供单人视图展示
    pub raw: VecDeque<String>,
    /// 每个议题里「上次活跃时该议题时间线的长度」,用于算增量前情。
    /// 必须按议题分开存:时间线是议题级的,而成员是跨议题共用的 ——
    /// 存成一个数字的话,在长议题里活跃过之后再去短议题干活,
    /// start 会等于 timeline.len(),整段前情被静默吞掉。
    pub last_seen: std::collections::HashMap<u64, usize>,
}

pub const RAW_CAP: usize = 2000; // 单 agent raw 环形缓冲上限

/// 单个成员待办队列上限。有上限是因为:一条我指令可以往每个成员塞一批活,
/// 队列不设顶的话在长会话里会单调增长(而且这些活越旧越没意义)。
/// 超出后丢**最旧**的,并让调用方报出来 —— 不能静默。
pub const INBOX_CAP: usize = 32;

/// 用户主动取消时 AgentDone.err 的内容。用它区分「用户掐的」和「真掉线」,
/// 两者在群聊里的措辞不一样。
pub const CANCELLED: &str = "已取消";

impl Member {
    /// 该成员在某议题里上次看到的时间线长度(没记过就是 0 = 从头看)。
    pub fn last_seen_for(&self, issue: u64) -> usize {
        self.last_seen.get(&issue).copied().unwrap_or(0)
    }
    /// 往待办队列里塞一条。超出上限时丢掉最旧的一条并返回它(供调用方提示用户)。
    pub fn push_inbox(&mut self, a: Assignment) -> Option<Assignment> {
        self.inbox.push_back(a);
        if self.inbox.len() > INBOX_CAP {
            self.inbox.pop_front()
        } else {
            None
        }
    }
    pub fn push_raw(&mut self, line: String) {
        self.raw.push_back(line);
        while self.raw.len() > RAW_CAP {
            self.raw.pop_front();
        }
    }
}

/// 一条待办派活。必须带 issue —— 排队期间用户可能切议题甚至关掉原议题，
/// 出队时不能再看「当前选中的议题」。
#[derive(Debug, Clone)]
pub struct Assignment {
    /// 这条派活属于哪个议题（Issue::id，不是索引也不是名字）
    pub issue: u64,
    pub text: String,
}

/// 群聊时间线里的一条消息。既是 UI，又是共享上下文。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMsg {
    pub ts: String,            // ISO 时间戳
    pub author: String,        // "我" / agent 名 / "系统"
    pub text: String,          // 展示文本（agent 的最终回复精炼 或 我原话）
    #[serde(default)]
    pub is_system: bool,       // 掉线/暂停等系统消息
}

/// 一个议题（tab）。属于项目，各有独立时间线。
#[derive(Debug, Clone)]
pub struct Issue {
    /// 进程内稳定唯一 id。索引会因增删议题而变、名字会被自动改名，
    /// 所以在跑的 agent 只认这个 id。
    pub id: u64,
    pub name: String,
    pub timeline: Vec<ChatMsg>,
    /// 本议题当前「一条我指令引发的 @ 连锁」轮数，用于防乒乓
    pub chain_depth: u32,
    /// 是否因防乒乓被暂停
    pub paused: bool,
}

static NEXT_ISSUE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 用一个已知 id 造议题（从盘上恢复时用），并把计数器推到它之后。
///
/// id 必须跨重启稳定：worktree 目录 `worktrees/<id>/` 和分支 `teamfly/issue-<id>`
/// 都按它命名。要是重启后重排，议题会去找不属于它的 worktree ——
/// 自己的改动成孤儿，还可能复用到别的议题留下的那个，两边改动混在一起。
pub fn issue_with_id(id: u64, name: impl Into<String>) -> Issue {
    NEXT_ISSUE_ID.fetch_max(id + 1, std::sync::atomic::Ordering::Relaxed);
    Issue {
        id,
        name: name.into(),
        timeline: Vec::new(),
        chain_depth: 0,
        paused: false,
    }
}

impl Issue {
    pub fn new(name: impl Into<String>) -> Self {
        Issue {
            id: NEXT_ISSUE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
    /// agent 环境变量集合(全局 + 按 backend 分段;来自 .teamfly/env.toml)
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
    /// 状态提示的过期 tick;超过后自动清除
    pub status_hint_until: u64,
    /// 待删除议题的确认状态:(议题索引, 按下时的 tick)。5s(约 33 tick)内再按 Ctrl+W 才真删。
    pub pending_delete: Option<(usize, u64)>,
    /// 二次确认时附带的「agent 改动会保留在哪个分支」说明。
    /// 在 update 里算好(要跑 git 查分支是否存在),渲染层只负责显示 ——
    /// 倒计时提示每帧重画,不能每帧 fork 一个 git 进程。
    pub pending_delete_note: String,
    /// 是否显示帮助浮层(? 键切换)
    pub show_help: bool,
    /// 取消令牌:Ctrl+C / 退出时用它掐掉所有在跑的 agent 子进程。
    /// 取消后会立刻换一个新 token,否则之后新起的 agent 一生下来就是取消态。
    pub cancel: tokio_util::sync::CancellationToken,
    /// 团队代号。每次 /team 热切 +1。在跑的 agent 带着派活时的代号,
    /// 代号过期的结果一律丢弃 —— 否则旧团队的汇报会按**新**花名册解析 @,
    /// 把新团队里同名的人莫名唤醒。
    pub team_gen: u64,
}

impl Model {
    pub fn cur_issue(&self) -> &Issue {
        &self.issues[self.current_issue]
    }
    pub fn cur_issue_mut(&mut self) -> &mut Issue {
        &mut self.issues[self.current_issue]
    }
    /// 按稳定 id 找议题下标。已被关闭则返回 None。
    pub fn issue_index(&self, id: u64) -> Option<usize> {
        self.issues.iter().position(|i| i.id == id)
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
    /// 鼠标点击顶部 tab 栏(col 是终端列号)
    MouseTabClick { col: u16 },
    /// 某 agent 进程吐了一行 raw（已剥 ANSI）
    AgentStdout { name: String, line: String },
    /// 某 agent 一轮结束
    AgentDone {
        name: String,
        /// 这一轮属于哪个议题(派活时绑定的 Issue::id)
        issue: u64,
        /// 派活时的团队代号。与当前 team_gen 不符则整条结果作废
        gen: u64,
        /// 这一轮用的 worktree (目录, 分支)。fallback 到主目录时为 None。
        /// 由派活时决定并原样回投 —— 不能事后去磁盘上猜,那样会拿到别轮的。
        worktree: Option<(std::path::PathBuf, String)>,
        /// 整轮 raw 汇总（用于兜底提取汇报）
        full_output: String,
        ok: bool,
        err: Option<String>,
    },
    /// spinner 定时器
    Tick,
    /// 落盘/删文件失败。以前这类错误被 `let _ =` 吞掉:磁盘满或目录没写权限时
    /// 界面完全正常、消息照样进时间线,但一条都没落盘,重开历史归零且零告警。
    IoError { detail: String },
}

/// update 返回的副作用描述，由 runtime 执行后回投 Msg。
#[derive(Debug)]
pub enum Command {
    /// 起一个 agent 进程干活：喂 prompt（含增量前情），流式回 AgentStdout，结束回 AgentDone
    SpawnAgent {
        name: String,
        /// 这一轮属于哪个议题(Issue::id)。结果回来时按它定位,不看「当前选中议题」
        issue: u64,
        /// 派活时的团队代号,原样回投
        gen: u64,
        backend: BackendKind,
        model: Option<String>,
        system_prompt: String,
        user_input: String, // 增量前情 + 本次指派
        /// 只读成员:主目录 + 无写权限
        read_only: bool,
    },
    /// 把一条群聊消息追加落盘。带 id:文件名是 <id>-<名字>.jsonl
    PersistChat { issue_id: u64, issue: String, msg: ChatMsg },
    /// 删除议题的落盘文件(关闭议题时)
    DeleteIssueFile { issue_id: u64, issue: String },
    /// 议题自动改名时,把落盘文件一起改名(保住已经落进去的内容)
    RenameIssueFile { issue_id: u64, from: String, to: String },
}