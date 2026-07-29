//! TUI 渲染:全屏四区 —— 顶部议题 tab / 左栏(#群聊+成员) / 右区(时间线 or raw 流) / 底部输入 + 快捷键条。
//! 纯渲染,读 Model 画 UI,不改状态。

use crate::model::{AgentState, Model, Selection};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [&str; 4] = ["⣾", "⣽", "⣻", "⢿"];

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

pub fn draw(f: &mut Frame, m: &Model) {
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

    draw_main(f, right[0], m);
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

    // 议题过多时,以当前议题为中心,只画左右各 2 个;溢出用 «/» 提示
    let total = m.issues.len();
    let (start, end, prefix, suffix) = if total <= 6 {
        (0, total, "", "")
    } else {
        let cur = m.current_issue;
        let s = cur.saturating_sub(2);
        let e = (cur + 3).min(total);
        (s, e, if s > 0 { "« " } else { "" }, if e < total { " »" } else { "" })
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
    spans.push(Span::styled("[+ 新议题]", Style::default().fg(Color::DarkGray)));

    // 左侧 tab
    f.render_widget(Paragraph::new(Line::from(spans)), inner);

    // 右端:工作目录(短路径),右对齐
    let wd = short_path(&m.work_dir);
    let right = Line::from(vec![Span::styled(
        format!("📂 {wd} "),
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(
        Paragraph::new(right).alignment(ratatui::layout::Alignment::Right),
        inner,
    );
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

fn draw_main(f: &mut Frame, area: Rect, m: &Model) {
    if area.width < MIN_CONTENT_W || area.height == 0 {
        // 只画 ASCII,避免双宽字符再踩上面那个坑
        f.render_widget(
            Paragraph::new("<>").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    match m.selection {
        Selection::Chat => draw_timeline(f, area, m),
        Selection::Member(i) => draw_agent_raw(f, area, m, i),
    }
}

fn draw_timeline(f: &mut Frame, area: Rect, m: &Model) {
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
            lines.push(Line::styled(
                format!("  {seg}"),
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
    m.scroll_max.set(total.saturating_sub(area.height));
    let scroll = bottom_scroll(total, area.height, m.scroll);
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn draw_agent_raw(f: &mut Frame, area: Rect, m: &Model, idx: usize) {
    let mem = &m.members[idx];
    let mut lines: Vec<Line> = Vec::new();
    let mut seen_init = false;
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
            continue;
        }
        let (style, prefix) = if l.starts_with("⟨err⟩") {
            (Style::default().fg(Color::Red), "  ")
        } else if l.starts_with("🔧") {
            (Style::default().fg(Color::Cyan), "  ")
        } else if l.starts_with("📋") {
            // 工具结果:挂在工具调用下面,再缩一层 + 灰色弱化
            (Style::default().fg(Color::DarkGray), "    ")
        } else if l.starts_with("❌") {
            // 工具执行失败
            (Style::default().fg(Color::Red), "    ")
        } else if l.starts_with("💭") {
            // 思考链:黄色 + dim,和回复正文拉开
            (Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM), "  ")
        } else {
            (Style::default().fg(Color::Gray), "  ")
        };
        lines.push(Line::styled(format!("{prefix}{l}"), style));
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
    m.scroll_max.set(total.saturating_sub(area.height));
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
    let cx = area.x + 3 + display_width(&m.input) as u16;
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
        const WINDOW: u64 = 33; // 与 handle_close_issue 中一致
        let elapsed = m.tick.wrapping_sub(t0);
        if elapsed < WINDOW && idx < m.issues.len() {
            let remain_ticks = WINDOW - elapsed;
            let remain_secs = (remain_ticks * 150) / 1000 + 1;
            Some(format!(
                "议题「{}」有 {} 条消息;再按 ^W 删除(剩 {}s)",
                m.issues[idx].name,
                m.issues[idx].timeline.len(),
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

fn display_width(s: &str) -> usize {
    // 粗略:CJK 记 2 宽,其余 1
    s.chars()
        .map(|c| if (c as u32) > 0x1100 { 2 } else { 1 })
        .sum()
}

fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = if (c as u32) > 0x1100 { 2 } else { 1 };
        if w + cw > width {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    if !cur.is_empty() {
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
            term.draw(|f| draw(f, &m))
                .unwrap_or_else(|e| panic!("{w}x{h} 渲染失败: {e}"));
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
        term.draw(|f| draw(f, &m)).unwrap();
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
                term.draw(|f| draw(f, &m))
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

