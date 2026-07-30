//! TUI 渲染:全屏四区 —— 顶部议题 tab / 左栏(#群聊+成员) / 右区(时间线 or raw 流) / 底部输入 + 快捷键条。
//! 纯渲染,读 Model 画 UI,不改状态。渲染层算出的度量(如内容高度)通过 DrawInfo 返回给主循环。

use crate::model::{AgentState, Model, Selection};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [&str; 4] = ["⣾", "⣽", "⣻", "⢿"];

/// 渲染层产出的度量,由主循环消费。保持 Model 是纯数据,渲染不回写。
#[derive(Debug, Default)]
pub struct DrawInfo {
    /// 右区内容在当前宽度下最多能往上滚多少行。
    /// 主循环用它夹 `model.scroll`,防止在短内容上连按 PageUp 把视图钉死在顶部。
    pub scroll_max: u16,
}

/// 把一个「内缩 1 行 1 列」的子区域裁到 `area` 内部。
///
/// 直接写 `Rect::new(area.x+1, area.y+1, ...)` 在终端只有 1 行高时会指到 buffer 外面
/// (`Layout` 会把 `Length(3)` 夹成 1 行,而 y+1 已经越界),ratatui 写 cell 时直接 panic。
/// 返回空 Rect 时调用方应当什么都不画。
fn inset(area: Rect, dx: u16, dy: u16, shrink_w: u16) -> Rect {
    if area.height <= dy || area.width <= shrink_w {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect::new(
        area.x + dx,
        area.y + dy,
        area.width.saturating_sub(shrink_w),
        1,
    )
}

pub fn draw(f: &mut Frame, m: &Model, info: &mut DrawInfo) {
    // 顶行(高 3):品牌角 | 议题标签栏  ——  下方:左栏 | 右列
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // 顶部:品牌角 + 议题栏(带边框)
            Constraint::Min(3),    // 主体
        ])
        .split(f.area());

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_W), Constraint::Min(20)])
        .split(root[0]);

    draw_topbar_frame(f, root[0]);
    draw_brand(f, top[0], m);
    draw_tabs(f, top[1], m);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_W), Constraint::Min(20)])
        .split(root[1]);

    draw_sidebar(f, body[0], m);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // 主区
            Constraint::Length(3), // 输入框
            Constraint::Length(1), // 快捷键条
        ])
        .split(body[1]);

    draw_main(f, right[0], m, info);
    draw_input(f, right[1], m);
    draw_hints(f, right[2], m);

    // 帮助浮层(最后画,覆盖在上面)
    if m.show_help {
        draw_help_overlay(f);
    }
}

/// 帮助浮层里的内容行数(和下面 `lines` 向量对齐;改了那个向量记得改这里)。
const HELP_LINES: u16 = 23;

/// 帮助浮层:居中显示所有键位,再按 ? 或 Esc 关闭。
fn draw_help_overlay(f: &mut Frame) {
    let area = f.area();
    let w = 66u16.min(area.width.saturating_sub(4));
    // 高度按内容条数算,不写死 —— 写死 20 时最后 3 行(^P / ^C)被裁在框外,
    // 而系统消息恰恰在教用户「按 Ctrl+P 恢复」,他按 ? 去查却查不到。
    let content_h = HELP_LINES + 2; // +2 是上下边框
    let h = content_h.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    // 先用 Clear 擦掉底下内容
    f.render_widget(ratatui::widgets::Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " 帮助(? / Esc 关闭)",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  发言", Style::default().add_modifier(Modifier::BOLD).fg(Color::White))]),
        Line::from("    输入文字 + ⏎     发到总览(不带 @ 只是留言)"),
        Line::from("    @名字 …           派活给该 agent(TAB 补全)"),
        Line::from("    Backspace / ^U    删字符 / 清空输入行"),
        Line::from(""),
        Line::from(vec![Span::styled("  斜杠命令", Style::default().add_modifier(Modifier::BOLD).fg(Color::White))]),
        Line::from("    /team <名>        切当前议题的团队"),
        Line::from(""),
        Line::from(vec![Span::styled("  切换视图", Style::default().add_modifier(Modifier::BOLD).fg(Color::White))]),
        Line::from("    ↑ ↓               选左栏(#总览 / 各成员;选中即显示)"),
        Line::from("    Esc               回 #总览"),
        Line::from(""),
        Line::from(vec![Span::styled("  议题", Style::default().add_modifier(Modifier::BOLD).fg(Color::White))]),
        Line::from("    ^N                新建议题(自动命名,第一条消息决定名字)"),
        Line::from("    ^W                关当前议题(有内容需再按一次确认)"),
        Line::from("    ^1..9 / Alt+1..9  切议题(^数字 部分终端不发送)"),
        Line::from(""),
        Line::from(vec![Span::styled("  其他", Style::default().add_modifier(Modifier::BOLD).fg(Color::White))]),
        Line::from("    PgUp / PgDn       上下翻历史"),
        Line::from("    ^P                解除防乒乓暂停并放出排队的活"),
        Line::from("    ^C                有 agent 在跑时:取消它们;否则退出"),
        Line::from("    鼠标              点左栏切视图 · 点顶栏切议题"),
    ];
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

const SIDEBAR_W: u16 = 20;

/// 顶部整条边框(横跨全宽),并在左栏右界画一条竖分隔 + T 型接头。
fn draw_topbar_frame(f: &mut Frame, area: Rect) {
    use ratatui::widgets::BorderType;
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(outer, area);

    // 在 x = area.x + SIDEBAR_W - 1 处画竖线(与左栏 Borders::RIGHT 同列)
    if area.width > SIDEBAR_W && area.height >= 3 {
        let x = area.x + SIDEBAR_W - 1;
        let top = area.y;
        let bot = area.y + area.height - 1;
        let mid = area.y + 1;
        let style = Style::default().fg(Color::DarkGray);
        let put = |f: &mut Frame, x: u16, y: u16, ch: &str| {
            let r = Rect::new(x, y, 1, 1);
            f.render_widget(Paragraph::new(ch).style(style), r);
        };
        put(f, x, top, "┬");
        put(f, x, mid, "│");
        put(f, x, bot, "┴");
    }
}

/// 左上角品牌块(无边框,边框由 topbar_frame 统一画)。
fn draw_brand(f: &mut Frame, area: Rect, _m: &Model) {
    let inner = inset(area, 1, 1, 2);
    if inner.width == 0 {
        return; // 终端太小/太矮,这块直接不画
    }
    let line = Line::from(vec![
        Span::styled("✦ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            "teamfly",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_tabs(f: &mut Frame, area: Rect, m: &Model) {
    // 边框由 topbar_frame 统一画;分隔线在 area 左侧前一列。
    // 内容从 area 左缘起(紧贴分隔线),右侧留 1 列给外框右边。
    let inner = inset(area, 0, 1, 1);
    if inner.width == 0 {
        return; // 终端太小/太矮,这块直接不画
    }
    let working = m.working_count();

    // 右端先给工作目录(短路径)划一块独立列区,tab 和它各占一段、绝不重叠。
    // 以前两者都往同一个 inner 里画,中等宽度下右对齐的路径会盖掉 [+ 新议题]
    // 的尾巴,渲染出 "[+ " / "新议题📂" 这种残缺碎片。
    let path_line = Line::from(vec![Span::styled(
        format!("📂 {} ", short_path(&m.work_dir)),
        Style::default().fg(Color::DarkGray),
    )]);
    // 至少给 tab 留 MIN_TABS 列(保住当前议题名);连有意义的路径都塞不下就整块不画。
    const MIN_TABS: u16 = 12;
    let mut path_w = (path_line.width() as u16 + 1).min(inner.width.saturating_sub(MIN_TABS));
    if path_w < 6 {
        path_w = 0;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(path_w)])
        .split(inner);
    let (tabs_area, path_area) = (cols[0], cols[1]);

    // 议题过多时,以当前议题为中心,只画左右各 2 个;溢出用 «/» 提示
    let total = m.issues.len();
    let (start, end, prefix, suffix) = if total <= 6 {
        (0, total, "", "")
    } else {
        let cur = m.current_issue;
        let s = cur.saturating_sub(2);
        let e = (cur + 3).min(total);
        (s, e, if s > 0 { "« " } else { "" }, if e < total { " » " } else { "" })
    };

    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));
    }
    for i in start..end {
        let issue = &m.issues[i];
        let badge = if i == m.current_issue && working > 0 {
            format!(" ⚙{working}")
        } else {
            String::new()
        };
        let paused = if issue.paused { " ⏸" } else { "" };
        let label = format!(" #{}{}{} ", issue.name, badge, paused);
        let style = if i == m.current_issue {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, Style::default().fg(Color::DarkGray)));
    }
    // [+ 新议题] 只有整块放得下才画 —— 被 tab 区右界截成 "[+ 新" 一样难看,
    // 宁可整块不画(新建议题还有 ^N 兜底)。
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let button = Span::styled("[+ 新议题]", Style::default().fg(Color::DarkGray));
    if used as u16 + button.width() as u16 <= tabs_area.width {
        spans.push(button);
    }

    f.render_widget(Paragraph::new(Line::from(spans)), tabs_area);
    if path_w > 0 {
        f.render_widget(
            Paragraph::new(path_line).alignment(ratatui::layout::Alignment::Right),
            path_area,
        );
    }
}

/// 缩短路径:只留最后两段,前面用 ~ 或 …。
fn short_path(p: &std::path::Path) -> String {
    let comps: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if comps.len() <= 2 {
        return p.display().to_string();
    }
    let tail = &comps[comps.len() - 2..];
    format!("…/{}/{}", tail[0], tail[1])
}

fn draw_sidebar(f: &mut Frame, area: Rect, m: &Model) {
    let mut lines: Vec<Line> = Vec::new();
    let frame = SPINNER[(m.tick as usize) % SPINNER.len()];

    // 团队名(头)
    lines.push(Line::from(vec![
        Span::styled("👥 ", Style::default().fg(Color::Cyan)),
        Span::styled(
            m.team_name.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::raw(""));

    // # 群聊 入口
    let chat_selected = m.selection == Selection::Chat;
    lines.push(sel_line("# 总览", chat_selected, Color::White));

    lines.push(Line::raw(""));

    // 成员
    for (i, mem) in m.members.iter().enumerate() {
        let selected = m.selection == Selection::Member(i);
        // 忙时用 spinner,闲时用状态 emoji
        let glyph = match mem.state {
            AgentState::Idle => "💤".to_string(),
            AgentState::Thinking => "💭".to_string(),
            AgentState::Working => frame.to_string(),
        };
        let head = format!("{} {} {}", mem.emoji, mem.name, glyph);
        lines.push(sel_line(&head, selected, state_color(mem.state)));
        let sub = format!("    {}·{}", mem.role, mem.state.label());
        lines.push(Line::styled(sub, Style::default().fg(Color::DarkGray)));
    }

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn sel_line(text: &str, selected: bool, color: Color) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let style = if selected {
        Style::default().fg(color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(color)
    };
    Line::styled(format!("{marker}{text}"), style)
}

fn state_color(s: AgentState) -> Color {
    match s {
        AgentState::Idle => Color::DarkGray,
        AgentState::Thinking => Color::Yellow,
        AgentState::Working => Color::Green,
    }
}

/// 正文区能正常渲染所需的最小宽度。
///
/// ratatui 0.28 在极窄区域里折行遇到双宽字符(中文/emoji)会写到 area 外面 ——
/// 实测宽 2 时会往 x=2 写,直接 `index outside of buffer` panic。
/// 这里的内容全是中文,所以宽度不够就换成纯 ASCII 的提示,不把内容喂进去。
const MIN_CONTENT_W: u16 = 4;

fn draw_main(f: &mut Frame, area: Rect, m: &Model, info: &mut DrawInfo) {
    if area.width < MIN_CONTENT_W || area.height == 0 {
        // 只画 ASCII,避免双宽字符再踩上面那个坑
        f.render_widget(
            Paragraph::new("<>").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    match m.selection {
        Selection::Chat => draw_timeline(f, area, m, info),
        Selection::Member(i) => draw_agent_raw(f, area, m, i, info),
    }
}

fn draw_timeline(f: &mut Frame, area: Rect, m: &Model, info: &mut DrawInfo) {
    let issue = m.cur_issue();
    let width = area.width.saturating_sub(1) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut last_date: Option<String> = None;

    for msg in &issue.timeline {
        // 跨天:插一行日期分隔
        let this_date = date_of(&msg.ts);
        if last_date.as_deref() != Some(&this_date) {
            lines.push(Line::styled(
                format!("── {this_date} ──"),
                Style::default().fg(Color::DarkGray),
            ));
            last_date = Some(this_date);
        }
        let (color, name) = if msg.is_system {
            (Color::Red, "⚠ 系统".to_string())
        } else if msg.author == "我" {
            (Color::Magenta, "🧑 我".to_string())
        } else {
            (Color::Green, msg.author.clone())
        };
        // 作者行:名字 + 右侧时间
        lines.push(Line::from(vec![
            Span::styled(
                format!("{name} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(short_ts(&msg.ts), Style::default().fg(Color::DarkGray)),
        ]));
        // 正文:缩进两格,像气泡
        for seg in wrap_text(&msg.text, width.saturating_sub(2)) {
            // 空段(段落之间的空行)**不能**加缩进 —— 纯空格的行会被 ratatui 的
            // WordWrapper 折成 2 行,而 wrapped_height 算它 1 行。每个空行少算
            // 一行,累积下来贴底偏移不够,底部内容被顶出可视区,而且 scroll_max
            // 同样算少,PageUp 也翻不到(现象是「多行汇报显示不全」)。
            let body = if seg.is_empty() { String::new() } else { format!("  {seg}") };
            lines.push(Line::styled(
                body,
                if msg.is_system {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::White)
                },
            ));
        }
        lines.push(Line::raw(""));
    }

    // 正在干活的 spinner 行
    let frame = SPINNER[(m.tick as usize) % SPINNER.len()];
    for mem in &m.members {
        if mem.state != AgentState::Idle {
            lines.push(Line::styled(
                format!("▸ {} {}… {}", mem.name, mem.state.label(), frame),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    if issue.paused {
        lines.push(Line::styled(
            "⏸ 已暂停(@ 连锁过深)。你可继续输入或按 Ctrl+P 恢复。",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    let block = Block::default().borders(Borders::NONE);
    let height = area.height as usize;
    // 群聊铁底:内容不足一屏时,顶部补空行把消息压到贴近输入框
    if lines.len() < height {
        let pad = height - lines.len();
        let mut padded = Vec::with_capacity(height);
        padded.extend(std::iter::repeat_with(|| Line::raw("")).take(pad));
        padded.append(&mut lines);
        lines = padded;
    }
    let total = wrapped_height(&lines, area.width);
    info.scroll_max = total.saturating_sub(area.height);
    let scroll = bottom_scroll(total, area.height, m.scroll);
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn draw_agent_raw(f: &mut Frame, area: Rect, m: &Model, idx: usize, info: &mut DrawInfo) {
    let mem = &m.members[idx];
    /// raw 流里一行属于哪一类。相邻两行**类别不同**时插一条空行,
    /// 让「思考 / 工具调用 / 工具结果 / 回复正文」在视觉上分块 ——
    /// 全挤在一起时扫读根本分不出边界。
    ///
    /// 工具结果(📋/❌)算和工具调用(🔧)同一块:结果本来就该紧挨着它的调用,
    /// 中间插空行反而把这对拆散了。
    #[derive(PartialEq, Clone, Copy)]
    enum Blk { Think, Tool, Text, Err }
    fn blk_of(l: &str) -> Blk {
        if l.starts_with("💭") {
            Blk::Think
        } else if l.starts_with("🔧") || l.starts_with("📋") || l.starts_with("❌") {
            Blk::Tool
        } else if l.starts_with("⟨err⟩") {
            Blk::Err
        } else if l.starts_with("   ") {
            // 多行工具结果的续行(stream 层用三空格对齐首行的图标)。
            // 判成 Text 的话会在结果中间插空行、还按正文着色 —— 结果被撕开。
            Blk::Tool
        } else {
            Blk::Text
        }
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut seen_init = false;
    let mut prev: Option<Blk> = None;
    for l in &mem.raw {
        // 每轮以 ⟨init⟩ 开头:新一轮前插一条空行 + 分隔,和上一轮拉开
        if l.starts_with("⟨init⟩") {
            if seen_init {
                lines.push(Line::raw(""));
            }
            seen_init = true;
            lines.push(Line::styled(
                format!("── {l} ──"),
                Style::default().fg(Color::DarkGray),
            ));
            // 分隔线本身也是一块:后面第一块内容要和它拉开。
            // `prev = None` 会让下一行不触发换块判断,于是分隔线和第一条思考
            // 紧贴 —— 别的块都分开了,只有这里没分反而更突兀。
            lines.push(Line::raw(""));
            prev = None;
            continue;
        }
        let kind = blk_of(l);
        // 空一行的两种情况:
        //  1. 换块了(思考 → 工具、工具 → 正文…)
        //  2. 同是工具块但又起了一次**新调用** —— 一串 🔧 连着会糊成一片,
        //     而 📋/❌ 要紧挨着它自己的 🔧,不能拆
        let new_block = prev.is_some_and(|p| p != kind);
        let new_tool_call = kind == Blk::Tool && prev == Some(Blk::Tool) && l.starts_with("🔧");
        if new_block || new_tool_call {
            lines.push(Line::raw(""));
        }
        prev = Some(kind);

        let (style, prefix) = match kind {
            Blk::Err => (Style::default().fg(Color::Red), "  "),
            Blk::Tool if l.starts_with("🔧") => (Style::default().fg(Color::Cyan), "  "),
            // 工具结果:挂在工具调用下面,再缩一层 + 灰色弱化
            Blk::Tool if l.starts_with("📋") => (Style::default().fg(Color::DarkGray), "    "),
            // 失败:整块红色(首行 ❌,续行三空格对齐)
            Blk::Tool if l.starts_with("❌") => (Style::default().fg(Color::Red), "    "),
            // 多行结果的续行:和它的首行同缩进同配色
            Blk::Tool => (Style::default().fg(Color::DarkGray), "    "),
            // 思考链:黄色 + dim,和回复正文拉开
            Blk::Think => (
                Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
                "  ",
            ),
            Blk::Text => (Style::default().fg(Color::Gray), "  "),
        };
        // 空行不加缩进:纯空格的行会被 WordWrapper 折成 2 行(见 word_wrap_rows),
        // 白占一行而且看不出来
        let body = if l.is_empty() { String::new() } else { format!("{prefix}{l}") };
        lines.push(Line::styled(body, style));
    }
    // 正在思考/干活:加一行 spinner
    let frame = SPINNER[(m.tick as usize) % SPINNER.len()];
    match mem.state {
        AgentState::Thinking => {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("💭 {} 思考中… {frame}", mem.name),
                Style::default().fg(Color::Yellow),
            ));
        }
        AgentState::Working => {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("⚙ {} 干活中… {frame}", mem.name),
                Style::default().fg(Color::Green),
            ));
        }
        AgentState::Idle => {
            if lines.is_empty() {
                lines.push(Line::styled(
                    format!("({} 还没有输出;被 @ 时才会开工)", mem.name),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }
    let total = wrapped_height(&lines, area.width);
    info.scroll_max = total.saturating_sub(area.height);
    let scroll = bottom_scroll(total, area.height, m.scroll);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, m: &Model) {
    // NewIssue 模式:输入议题名,标题反映

    // @ 补全建议(输入中最后一个 @token 的候选)
    let roster: Vec<String> = m.members.iter().map(|x| x.name.clone()).collect();
    let sugg = crate::app::at_suggestions(&m.input, &roster);
    let title = if !sugg.is_empty() {
        format!(" @ 补全:{}  (Tab 选第一个) ", sugg.join(" · "))
    } else {
        " 输入(@名字 派活 · 不带@只是留言) ".to_string()
    };
    let border_color = if !sugg.is_empty() { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, Style::default().fg(border_color)));
    let text = format!("> {}", m.input);
    f.render_widget(Paragraph::new(text).block(block), area);
    // 光标
    let cx = area.x + 3 + str_cols(&m.input) as u16;
    let cy = area.y + 1;
    f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(2)), cy));
}

fn draw_hints(f: &mut Frame, area: Rect, m: &Model) {
    // 帮助浮层打开时,提示关闭方式
    if m.show_help {
        let hint = "「? / Esc 关闭帮助」 · 按其它键自动关闭并继续操作";
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }

    // pending_delete 未过期时,动态显示剩余秒(覆盖普通 hint)
    let dynamic_pending: Option<String> = m.pending_delete.and_then(|(idx, t0)| {
        const WINDOW: u64 = crate::app::DELETE_CONFIRM_TICKS;
        let elapsed = m.tick.wrapping_sub(t0);
        if elapsed < WINDOW && idx < m.issues.len() {
            let remain_ticks = WINDOW - elapsed;
            let remain_secs = (remain_ticks * 150) / 1000 + 1;
            Some(format!(
                "议题「{}」有 {} 条消息{};再按 ^W 删除(剩 {}s)",
                m.issues[idx].name,
                m.issues[idx].timeline.len(),
                m.pending_delete_note,
                remain_secs
            ))
        } else {
            None
        }
    });
    let hint = dynamic_pending.or_else(|| m.status_hint.clone()).unwrap_or_else(|| {
        "? 帮助 · ^N 新议题 · ^W 关议题 · Alt+1-9 切议题 · ⏎ 发送 · ^C 取消/退出".to_string()
    });
    let color = if m.pending_delete.is_some() {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(color)),
        area,
    );
}

// ---- 辅助 ----

fn short_ts(ts: &str) -> String {
    // 取 HH:MM
    if let Some(t) = ts.split('T').nth(1) {
        t.chars().take(5).collect()
    } else {
        ts.chars().take(5).collect()
    }
}

/// 从 ISO 时间戳里取日期部分 YYYY-MM-DD。
fn date_of(ts: &str) -> String {
    ts.split('T').next().unwrap_or("").to_string()
}

fn bottom_scroll(total: u16, height: u16, user_scroll: u16) -> u16 {
    if total <= height {
        0
    } else {
        (total - height).saturating_sub(user_scroll)
    }
}

/// 这批 Line 在宽度 `w` 下渲染成多少行。
///
/// 必须按**折行后**的行数算:Paragraph 开了 `Wrap` 时 `.scroll()` 的单位是渲染行,
/// 而 `lines.len()` 是未折行的条目数。只要有一条比区域宽,贴底偏移就会算少,
/// 底部内容被顶出可视区且再也划不下去(现象是「界面卡住不更新」)。
/// `Line::width()` 用的是 unicode-width,和 ratatui 内部折行的度量一致。
fn wrapped_height(lines: &[Line], w: u16) -> u16 {
    let w = w.max(1) as usize;
    let total: usize = lines.iter().map(|l| word_wrap_rows(l, w)).sum();
    total.min(u16::MAX as usize) as u16
}

/// 一条 Line 在宽度 w 下被 WordWrapper 折成几行。
///
/// 不能简单用 `width / w` 向上取整:`Wrap` 是**按词**折行,
/// 「若干短词 + 一个塞不进剩余宽度的长 token」会比整除多占一行。
/// raw 视图里全是这种(`🔧 Read(很长的路径)`、命令行),少算就会让底部划不到。
fn word_wrap_rows(line: &Line, w: usize) -> usize {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if text.is_empty() {
        return 1; // 空行也占一行
    }
    // **非空但全是空白**的行,WordWrapper 会吐出两行(空白那行 + 一个空行),
    // 只要那段空白放得下。实测:" "/"  "/"\t" 在宽 70 下都是 2 行,
    // 而 "     " 在宽 4(放不下)下是 1 行。
    //
    // 这个 quirk 必须照搬 —— 少算一行的话贴底偏移和 scroll_max 都偏小,
    // 底部内容被顶出可视区且 PageUp 也翻不到。调用方应尽量别产出这种行
    // (时间线的空段就不加缩进前缀),但 wrapped_height 得对任何输入都准。
    if text.chars().all(char::is_whitespace) {
        return if str_cols(&text) <= w { 2 } else { 1 };
    }
    let mut rows = 1usize;
    let mut used = 0usize; // 当前行已占列数
    // 逐字符放置:宽 2 的字符在只剩 1 列时会整体挪到下一行,
    // 所以不能用「列数整除宽度」来算硬切(中文长串会少算)。
    let put = |ch: char, rows: &mut usize, used: &mut usize| {
        let cw = char_cols(ch);
        if *used + cw > w && *used > 0 {
            *rows += 1;
            *used = 0;
        }
        *used += cw;
    };

    let mut first = true;
    let mut rest = text.as_str();
    while !rest.is_empty() {
        // 按「空白 + 非空白」成对推进,和 WordWrapper 的贪心一致
        let ws_len = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        let (ws, tail) = rest.split_at(ws_len);
        let word_len = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let (word, next) = tail.split_at(word_len);

        if first {
            // 行首空白保留(Wrap{trim:false} 的语义)
            for ch in ws.chars() {
                put(ch, &mut rows, &mut used);
            }
            first = false;
        } else if used > 0 && used + str_cols(ws) + str_cols(word) > w {
            // 断行。断行处的分隔空白会被**吃掉**,不带到下一行 ——
            // 否则每断一次多算一列,长句会整体多算出一行。
            rows += 1;
            used = 0;
        } else {
            for ch in ws.chars() {
                put(ch, &mut rows, &mut used);
            }
        }
        for ch in word.chars() {
            put(ch, &mut rows, &mut used);
        }
        rest = next;
    }
    rows
}

fn char_cols(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0)
}

fn str_cols(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// 把一段正文切成「每行一段」,供渲染层逐段加缩进前缀。
///
/// 两件事:
/// 1. **先按 `\n` 断行**。agent 的汇报几乎都是多行的(列表、代码块、分点),
///    而 ratatui 的 `Line` 不把内容里的 `\n` 当换行 —— 它会被当控制字符渲染成
///    空白,于是整段汇报在总览里挤成一坨。
/// 2. 再按宽度折行。这里不能交给 `Paragraph` 的 `Wrap` 做:折出来的续行不会
///    带上调用方加的两格缩进,气泡会破形。
///
/// 宽度用 unicode-width 逐字符算。以前是「码点 > 0x1100 就算 2 列」的启发式,
/// `…`/`①`/变体选择符全都算错(和 tab 热区、输入行光标那两处同源的 bug)。
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    // \r\n 和纯 \r 都当换行:agent 输出里两种都见过
    for para in s.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let mut cur = String::new();
        let mut w = 0usize;
        for c in para.chars() {
            // 控制字符没有宽度概念,直接跳过(制表符等留给上层的 raw 视图)
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(c);
            w += cw;
        }
        // 空段也要推:`\n\n` 中间那个空行是作者故意留的段落间隔
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// agent 汇报里的换行必须在总览里真的换行。
    ///
    /// ratatui 的 `Line` 不把内容里的 `\n` 当换行 —— 它被当控制字符渲染成空白,
    /// 于是多行汇报(列表、分点、代码块)在总览里挤成一坨。agent 的汇报几乎
    /// 都是多行的,所以这条影响每一次交卷。
    #[test]
    fn timeline_renders_newlines_as_line_breaks() {
        let segs = wrap_text("第一行\n第二行\n\n第四行", 40);
        assert_eq!(
            segs,
            vec!["第一行", "第二行", "", "第四行"],
            "换行没被切成独立段"
        );
        // \r\n 和裸 \r 也算换行
        assert_eq!(wrap_text("a\r\nb", 40), vec!["a", "b"]);
        assert_eq!(wrap_text("a\rb", 40), vec!["a", "b"]);
    }

    /// 端到端:真的画一帧,确认多行汇报在屏幕上占了多行。
    #[test]
    fn multiline_report_occupies_multiple_screen_rows() {
        const W: u16 = 80;
        const H: u16 = 24;
        let mut m = crate::app::test_support::tiny_model();
        m.issues[0].timeline.push(crate::model::ChatMsg {
            ts: "2026-07-30T10:00:00".into(),
            author: "DEV".into(),
            text: "改了三个文件:\n- src/a.rs\n- src/b.rs\n- src/c.rs".into(),
            is_system: false,
        });
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        term.draw(|f| draw(f, &m, &mut DrawInfo::default())).unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..H)
            .map(|y| (0..W).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();

        // 三个文件名必须各占一行,不能挤在同一行
        let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle));
        let (ra, rb, rc) = (
            row_of("src/a.rs").expect("a 没画出来"),
            row_of("src/b.rs").expect("b 没画出来"),
            row_of("src/c.rs").expect("c 没画出来"),
        );
        assert!(
            ra < rb && rb < rc,
            "三行挤在一起了:a在{ra}行 b在{rb}行 c在{rc}行\n{}",
            rows.join("\n")
        );
    }

    /// `wrapped_height` 必须和真实渲染逐行一致 —— **包括只含空格的行**。
    ///
    /// 纯空格的行会被 ratatui 的 WordWrapper 折成 2 行,而 wrapped_height
    /// 算它 1 行。汇报里每个段落间的空行都会踩到:每个少算一行,累积下来
    /// 贴底偏移不够、scroll_max 也偏小,底部内容被顶出屏幕且 PageUp 翻不到。
    #[test]
    fn height_matches_for_whitespace_only_lines() {
        for (txt, w) in [("", 70u16), ("  ", 70), (" ", 70), ("   ", 10), ("\t", 20)] {
            let got = wrapped_height(&[Line::raw(txt)], w);
            let real = real_rows(txt, w) as u16;
            assert_eq!(got, real, "{txt:?} 宽{w}: 算 {got} 行,实际渲染 {real} 行");
        }
    }

    /// 时间线里的空行不能带缩进前缀 —— 否则就成了上面那种「纯空格行」。
    #[test]
    fn timeline_blank_lines_carry_no_indent() {
        const W: u16 = 70;
        const H: u16 = 24;
        let mut m = crate::app::test_support::tiny_model();
        m.members.push(crate::app::test_support::tiny_member("DEV"));
        m.issues[0].timeline.push(crate::model::ChatMsg {
            ts: "2026-07-30T10:00:00".into(),
            author: "DEV".into(),
            text: "第一段\n\n第二段\n\n第三段".into(),
            is_system: false,
        });
        let mut di = DrawInfo::default();
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        t.draw(|f| draw(f, &m, &mut di)).unwrap();
        let b = t.backend().buffer().clone();
        // 右区里「看起来空」的行必须真的一个字符都没有(不能是两个空格)
        for y in 0..H {
            let raw: String = (21..W).map(|x| b[(x, y)].symbol()).collect();
            if raw.trim().is_empty() {
                assert!(
                    !raw.starts_with("  ") || raw.chars().all(|c| c == ' '),
                    "第 {y} 行是带缩进的空行"
                );
            }
        }
        // 真正的断言:三段都在,且 wrapped_height 和真实渲染一致
        let all: String = (0..H)
            .map(|y| (21..W).map(|x| b[(x, y)].symbol()).collect::<String>())
            .collect();
        let flat: String = all.chars().filter(|c| !c.is_whitespace()).collect();
        for seg in ["第一段", "第二段", "第三段"] {
            assert!(flat.contains(seg), "{seg} 没显示出来");
        }
    }

    /// 用仓库里真实的 jsonl 走**完整加载路径**,确认多行汇报每一行都能看到。
    ///
    /// 这条盯的是 sample/.teamfly 里那条 8 行的 DEV 汇报 —— 换行被吃掉时
    /// 它会挤成一坨(见 #24)。前面那些测试用的是手写文本,这条用真数据。
    ///
    /// 注意两个读屏陷阱(都踩过):
    /// 1. 只能读**右区**(x>=21)。连左栏一起读的话,一句话折行后
    ///    左栏文字会被插进这句中间,`contains` 直接找不到。
    /// 2. 每个滚动位置要用**全新** TestBackend。同一个 backend 连续画多帧时,
    ///    宽字符(CJK)占两格,上一帧的第二格残留会和新帧交错成乱码。
    #[test]
    fn real_multiline_report_is_fully_reachable() {
        const W: u16 = 90;
        const H: u16 = 30;
        let tf = std::path::Path::new("sample/.teamfly");
        if !tf.join("issues").is_dir() {
            return; // 没有样例数据就跳过(别人 clone 下来可能没带)
        }
        let (issues, _) = crate::issue::load_all_issues(tf).unwrap();
        let Some(idx) = issues.iter().position(|i| {
            i.timeline.iter().any(|m| m.author == "DEV" && m.text.lines().count() > 3)
        }) else {
            return; // 样例里没有多行 DEV 汇报,这条就没东西可测
        };
        let want: Vec<String> = issues[idx]
            .timeline
            .iter()
            .filter(|m| m.author == "DEV")
            .flat_map(|m| m.text.lines())
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert!(want.len() >= 4, "样例数据该有几行才有意义");

        let mut m = crate::app::test_support::tiny_model();
        m.members.push(crate::app::test_support::tiny_member("DEV"));
        m.issues = issues;
        m.current_issue = idx;

        // 先画一帧拿 scroll_max
        let mut di = DrawInfo::default();
        let mut t0 =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        t0.draw(|f| draw(f, &m, &mut di)).unwrap();

        // 翻遍每个滚动位置,收集右区出现过的文本
        let mut seen = String::new();
        for step in 0..=di.scroll_max {
            m.scroll = step;
            let mut t =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
            t.draw(|f| draw(f, &m, &mut di)).unwrap();
            let b = t.backend().buffer().clone();
            for y in 0..H {
                seen.extend((21..W).map(|x| b[(x, y)].symbol()));
            }
        }
        // CJK 在 TestBackend 里逐字符占格,比对时去掉所有空白
        let flat: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        let missing: Vec<&String> = want
            .iter()
            .filter(|l| {
                let key: String = l.chars().filter(|c| !c.is_whitespace()).collect();
                !flat.contains(&key)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "翻遍 {} 个滚动位置仍看不到这些行:{missing:#?}",
            di.scroll_max + 1
        );
    }

    /// 多行工具结果在 raw 视图里必须逐行显示,且整块跟着它的 🔧 不被撕开。
    #[test]
    fn raw_view_shows_multiline_tool_result() {
        const W: u16 = 60;
        const H: u16 = 20;
        let mut m = crate::app::test_support::tiny_model();
        m.members.push(crate::app::test_support::tiny_member("DEV"));
        for l in [
            "⟨init⟩ model=x tools=1",
            "🔧 Read(Cargo.toml)",
            "📋 R1 first",
            "   R2 second",
            "   R3 third",
            "final-reply ok",
        ] {
            m.members[0].push_raw(l.into());
        }
        m.selection = crate::model::Selection::Member(0);

        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        t.draw(|f| draw(f, &m, &mut DrawInfo::default())).unwrap();
        let b = t.backend().buffer().clone();
        let rows: Vec<String> = (0..H)
            .map(|y| (21..W).map(|x| b[(x, y)].symbol()).collect::<String>())
            .map(|r| r.trim_end().to_string())
            .collect();
        let row_of = |n: &str| {
            rows.iter()
                .position(|r| r.contains(n))
                .unwrap_or_else(|| panic!("找不到 {n}\n{}", rows.join("\n")))
        };
        let (r1, r2, r3) = (row_of("R1"), row_of("R2"), row_of("R3"));
        // 三行结果各占一行,顺序不乱
        assert!(r1 < r2 && r2 < r3, "结果三行没有分行:{r1} {r2} {r3}");
        // 中间不能插空行 —— 那会把一块结果撕成几块
        assert!(
            rows[r1 + 1..r3].iter().all(|r| !r.is_empty()),
            "多行结果中间被插了空行\n{}",
            rows.join("\n")
        );
        // 结果和它的 🔧 之间也不空
        let tool = row_of("Read(");
        assert!(
            rows[tool + 1..r1].iter().all(|r| !r.is_empty()),
            "结果和它的工具调用被拆开了"
        );
        // 但结果块和后面的正文之间要空
        let txt = row_of("final-reply");
        assert!(
            rows[r3 + 1..txt].iter().any(|r| r.is_empty()),
            "结果块和正文之间该空一行"
        );
    }

    /// raw 视图里「思考 / 工具调用 / 回复正文」之间必须空行分块。
    ///
    /// 全紧贴在一起时扫读分不出边界 —— 一屏几十行里哪几行是思考、哪几行是
    /// 工具调用、agent 最后说了什么,全糊成一片。
    ///
    /// 但**工具结果要紧挨着它的调用**(📋/❌ 跟在 🔧 后面),中间插空行反而
    /// 把这一对拆散了。
    #[test]
    fn raw_view_separates_semantic_blocks() {
        const W: u16 = 70;
        const H: u16 = 26;
        let mut m = crate::app::test_support::tiny_model();
        m.members.push(crate::app::test_support::tiny_member("DEV"));
        for l in [
            "⟨init⟩ model=x tools=3",
            "💭 thinking-one auth.py",
            "💭 thinking-two decide",
            "🔧 Read(src/auth.py)",
            "📋 res-read 42",
            "🔧 Edit(src/auth.py)",
            "📋 res-edit ok",
            "final-reply done",
        ] {
            m.members[0].push_raw(l.into());
        }
        m.selection = crate::model::Selection::Member(0);

        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        t.draw(|f| draw(f, &m, &mut DrawInfo::default())).unwrap();
        let b = t.backend().buffer().clone();
        // 只取右区(左栏宽 20),按行取文本
        let rows: Vec<String> = (0..H)
            .map(|y| (21..W).map(|x| b[(x, y)].symbol()).collect::<String>())
            .map(|r| r.trim_end().to_string())
            .collect();
        let row_of = |needle: &str| {
            rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| {
                panic!("找不到 {needle}\n{}", rows.join("\n"))
            })
        };
        let blank_between = |a: usize, b: usize| rows[a + 1..b].iter().any(|r| r.is_empty());

        let (t1, t2) = (row_of("thinking-one"), row_of("thinking-two"));
        let (r1, res1) = (row_of("Read("), row_of("res-read"));
        let (e1, res2) = (row_of("Edit("), row_of("res-edit"));
        let txt = row_of("final-reply");

        // 连续两条思考之间不空行
        assert!(!blank_between(t1, t2), "同类之间不该空行\n{}", rows.join("\n"));
        // 思考 → 工具:空
        assert!(blank_between(t2, r1), "思考和工具调用之间该空一行\n{}", rows.join("\n"));
        // 工具调用和它的结果之间:不空(结果要挂在调用下面)
        assert!(!blank_between(r1, res1), "工具结果被和它的调用拆开了\n{}", rows.join("\n"));
        // 两次工具调用之间:空
        assert!(blank_between(res1, e1), "两次工具调用之间该空一行\n{}", rows.join("\n"));
        assert!(!blank_between(e1, res2), "工具结果被拆开了");
        // 工具 → 正文:空
        assert!(blank_between(res2, txt), "工具块和回复正文之间该空一行\n{}", rows.join("\n"));
        // ⟨init⟩ 分隔线和它后面第一块之间也要空 —— 别的块都分开了,
        // 只有分隔线贴着后面的内容会更突兀
        let sep = row_of("⟨init⟩");
        assert!(
            blank_between(sep, t1),
            "⟨init⟩ 分隔线后面该空一行\n{}",
            rows.join("\n")
        );
    }

    /// 折行宽度也得用 unicode-width。这是同一个错误启发式的第三份拷贝
    /// (tab 热区、输入行光标那两处已经修过),`…`/`①` 被算成 2 列的话
    /// 每行会比实际窄,气泡右边缘参差不齐。
    #[test]
    fn wrap_uses_real_width_not_heuristic() {
        // 40 个 `…`:真实宽度 40 列,刚好一行装满
        let dots: String = "…".repeat(40);
        assert_eq!(wrap_text(&dots, 40).len(), 1, "按真实宽度该正好一行");
        // CJK 确实是 2 列
        assert_eq!(wrap_text(&"汉".repeat(20), 40).len(), 1, "20 个汉字刚好 40 列");
        assert_eq!(wrap_text(&"汉".repeat(21), 40).len(), 2, "多一个就该折行");
    }

    /// 输入行光标必须按**终端实际列宽**算,不能用「码点 > 0x1100 就算 2 列」的启发式。
    /// 那个启发式把 `…`/`①` 算成 2 列、把带变体选择符的 emoji 算成 4 列,
    /// 用户一打这些字符光标就飘到字的右边去。
    #[test]
    fn cursor_uses_real_terminal_width() {
        assert_eq!(str_cols("你好"), 4);   // CJK 确实是 2 列
        assert_eq!(str_cols("…"), 1);      // 启发式会说 2
        assert_eq!(str_cols("①"), 1);      // 启发式会说 2
        assert_eq!(str_cols("🛡\u{FE0F}"), 2); // 启发式会说 4(变体选择符也被算成 2)
    }

    /// 极小终端不能 panic。
    ///
    /// 以前 draw_brand / draw_tabs 直接算 `area.y + 1`:终端只有 1 行高时
    /// `Layout` 会把 `Length(3)` 夹成 1 行,y+1 已在 buffer 外,ratatui 写 cell 时直接 panic ——
    /// 而 panic 会绕过终端恢复,把用户留在花屏里。
    #[test]
    fn draw_survives_tiny_terminals() {
        for (w, h) in [(1, 1), (2, 1), (80, 1), (80, 2), (80, 3), (1, 40), (5, 5), (20, 8)] {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let m = crate::app::test_support::tiny_model();
            term.draw(|f| draw(f, &m, &mut DrawInfo::default()))
                .unwrap_or_else(|e| panic!("{w}x{h} 渲染失败: {e}"));
        }
    }

    /// 回归:中等宽度下,右对齐的工作目录路径曾盖掉 [+ 新议题] 按钮尾部,
    /// 渲染出 "[+ " / "新议题📂" 这种残缺碎片(tab 行和路径画进了同一个 rect)。
    /// 现在两者各占一段列区:按钮要么整块画、要么整块不画,路径始终完整。
    #[test]
    fn tab_button_and_path_never_collide() {
        let mut m = crate::app::test_support::tiny_model();
        m.work_dir = std::path::PathBuf::from("/home/u/proj/teamfly");
        m.issues[0].name = "默认议题".into();
        for w in [56u16, 58, 60, 62, 66, 70, 80, 120] {
            // 高度给足 6:top(3) 满足后 body 拿剩下 3,tab 栏才真的画得出来
            // (只给 3 行时约束求解器会把固定高的顶栏压成 0,body 顶上来)。
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 6)).unwrap();
            term.draw(|f| draw(f, &m, &mut DrawInfo::default())).unwrap();
            let buf = term.backend().buffer().clone();
            // tab 内容在顶框内那一行(y=1);整行拼起来后去掉空白便于比对
            // (TestBackend 把双宽字的续格存成空,去空白对 "" / " " 两种都稳)
            let row: String = (0..w).map(|x| buf[(x, 1)].symbol()).collect();
            let flat: String = row.chars().filter(|c| !c.is_whitespace()).collect();
            // 按钮要么完整,要么根本没画 —— 绝不能是被截断/覆盖的碎片
            if flat.contains("[+") {
                assert!(flat.contains("[+新议题]"), "宽 {w}:按钮成了残缺碎片:{row:?}");
            }
            // 路径图标始终画得出(和 tab 各占一段,不互相吃掉)
            assert!(flat.contains("📂"), "宽 {w}:路径被挤没了:{row:?}");
        }
    }

    /// 帮助浮层不能被裁:内容行数必须和 HELP_LINES 一致,而且在常见终端里放得下。
    /// 以前高度写死 20 而内容 21 行,最后几行(^P / ^C)永远看不到 ——
    /// 而系统消息恰恰在教用户按 Ctrl+P。
    #[test]
    fn help_overlay_not_truncated() {
        const H: u16 = 40;
        const W: u16 = 100;
        let mut m = crate::app::test_support::tiny_model();
        m.show_help = true;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, H)).unwrap();
        term.draw(|f| draw(f, &m, &mut DrawInfo::default())).unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..H)
            .map(|y| (0..W).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        // TestBackend 把双宽字符存成「字符 + 空续格」,重建出来会多空格,比较前先去掉
        let flat: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        for must in ["^P", "^C", "PgUp", "鼠标"] {
            let needle: String = must.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(flat.contains(&needle), "帮助浮层里看不到 {must},说明被裁了");
        }
    }

    /// 极窄终端 + 长 CJK 行:折行会落在双宽字符中间,ratatui 在 area.right() 处越界。
    #[test]
    fn draw_survives_narrow_terminal_with_cjk() {
        let long = "@ 连锁已达 1 轮,自动暂停以防打转。按 Ctrl+P 恢复,或直接发新指令。";
        for w in [1u16, 2, 3, 4, 5] {
            for h in [8u16, 17, 24, 40] {
                let mut m = crate::app::test_support::tiny_model();
                m.members.push(crate::app::test_support::tiny_member("老K"));
                m.selection = Selection::Member(0);
                m.members[0].raw.push_back(long.to_string());
                m.issues[0].timeline.push(crate::model::ChatMsg {
                    ts: "t".into(),
                    author: "老K".into(),
                    text: long.to_string(),
                    is_system: false,
                });
                let mut term =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                term.draw(|f| draw(f, &m, &mut DrawInfo::default()))
                    .unwrap_or_else(|e| panic!("{w}x{h} 渲染失败: {e}"));
            }
        }
    }

    /// `text` 在宽度 w 下真实占了几行。
    ///
    /// 做法:在它后面放一个哨兵行,看哨兵落到第几行 —— 那就是 text 占的行数。
    /// 不能数「非空行」:空行渲染出来什么都没有,但它在滚动坐标里确实占一行。
    fn real_rows(text: &str, w: u16) -> usize {
        const H: u16 = 60;
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, H)).unwrap();
        t.draw(|f| {
            f.render_widget(
                Paragraph::new(vec![Line::raw(text), Line::raw("\u{2588}")])
                    .wrap(Wrap { trim: false }),
                f.area(),
            )
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        (0..H)
            .find(|y| (0..w).any(|x| buf[(x, *y)].symbol() == "\u{2588}"))
            .expect("哨兵行应该在可视区内") as usize
    }

    /// 折行后的高度必须和 ratatui 的 WordWrapper 一致。
    ///
    /// 用 `width / w` 向上取整会算少:按词折行时「短词 + 塞不进的长 token」
    /// 会多占一行。算少 → 贴底偏移偏小 → 最新输出被顶出可视区且划不回来。
    #[test]
    fn wrapped_height_matches_real_rendering() {
        let cases: &[(&str, u16)] = &[
            ("abcdefghij", 10),
            ("abcdefghij", 5),
            ("abcdefghij", 3),
            ("", 10),
            ("中文", 2),
            ("12345 67890 12345", 10),
            ("  🔧 Read(/home/user/project/src/some/deep/path/module.rs)", 40),
            ("📋 1 [package] 2 name = \"teamfly\" 3 version = \"0.1.0\"", 20),
            ("💭 The user wants me to read two files and then reply", 24),
            ("a b c d e f g h i j k l m n o p", 7),
            ("单个超长中文词汇没有空格所以只能硬切", 9),
            ("short", 80),
        ];
        for (txt, w) in cases {
            let got = wrapped_height(&[Line::raw(*txt)], *w);
            let want = real_rows(txt, *w) as u16;
            assert_eq!(got, want, "宽{w} 文本{txt:?}: 算出 {got} 行,实际渲染 {want} 行");
        }
    }
}

