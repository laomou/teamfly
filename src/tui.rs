//! TUI 渲染:全屏四区 —— 顶部议题 tab / 左栏(#群聊+成员) / 右区(时间线 or raw 流) / 底部输入 + 快捷键条。
//! 纯渲染,读 Model 画 UI,不改状态。

use crate::model::{AgentState, Model, Selection};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [&str; 4] = ["⣾", "⣽", "⣻", "⢿"];

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
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        1,
    );
    let line = Line::from(vec![
        Span::styled("✦ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            "teamfly",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_tabs(f: &mut Frame, area: Rect, m: &Model) {
    // 边框由 topbar_frame 统一画;分隔线在 area 左侧前一列。
    // 内容从 area 左缘起(紧贴分隔线),右侧留 1 列给外框右边。
    let inner = Rect::new(
        area.x,
        area.y + 1,
        area.width.saturating_sub(1),
        1,
    );
    let working = m.working_count();
    let mut spans = Vec::new();
    for (i, issue) in m.issues.iter().enumerate() {
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
    lines.push(sel_line("# 群聊", chat_selected, Color::White));

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

fn draw_main(f: &mut Frame, area: Rect, m: &Model) {
    match m.selection {
        Selection::Chat => draw_timeline(f, area, m),
        Selection::Member(i) => draw_agent_raw(f, area, m, i),
    }
}

fn draw_timeline(f: &mut Frame, area: Rect, m: &Model) {
    let issue = m.cur_issue();
    let width = area.width.saturating_sub(1) as usize;
    let mut lines: Vec<Line> = Vec::new();

    for msg in &issue.timeline {
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
    let total = lines.len() as u16;
    let scroll = bottom_scroll(total, area.height, m.scroll);
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn draw_agent_raw(f: &mut Frame, area: Rect, m: &Model, idx: usize) {
    let mem = &m.members[idx];
    let mut lines: Vec<Line> = Vec::new();
    for l in &mem.raw {
        let style = if l.starts_with("⟨err⟩") {
            Style::default().fg(Color::Red)
        } else if l.contains(crate::router::CHAT_MARK) {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::styled(l.clone(), style));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            format!("({} 还没有输出;被 @ 时才会开工)", mem.name),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let total = lines.len() as u16;
    let scroll = bottom_scroll(total, area.height, m.scroll);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, m: &Model) {
    // NewIssue 模式:输入议题名,标题反映
    if m.input_mode == crate::model::InputMode::NewIssue {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " 新建议题 · ⏎ 创建 · Esc 取消 ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        let text = format!("# {}", m.input);
        f.render_widget(Paragraph::new(text).block(block), area);
        let cx = area.x + 3 + display_width(&m.input) as u16;
        let cy = area.y + 1;
        f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(2)), cy));
        return;
    }

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
    let hint = m.status_hint.clone().unwrap_or_else(|| {
        "^N 新议题 · ^W 关议题 · ^1-9 切议题 · ↑↓ 选左栏 · ⏎ 发送/看TA · ^C 退出".to_string()
    });
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
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

fn bottom_scroll(total: u16, height: u16, user_scroll: u16) -> u16 {
    if total <= height {
        0
    } else {
        (total - height).saturating_sub(user_scroll)
    }
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
