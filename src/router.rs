//! 路由：从 agent 的最终回复(result)提炼群聊汇报，解析 @ 指派，防乒乓。
//! 尽量纯函数，便于单测。

/// 群聊汇报的**展示**字数上限(超出截断)。注意:只影响群聊里怎么显示,
/// 不影响 @ 解析和投递给下游的内容 —— 那两件事必须用完整文本。
const REPORT_MAX_CHARS: usize = 500;

/// 从 agent 一轮的最终回复(claude/codex 的 result 文本)提炼成汇报。
///
/// 无需 agent 写任何标记 —— result 本就是它这轮的收尾回复。
/// 做的清理:去掉首尾空白与空行、折叠连续空行。
///
/// **不截断**:团队规约要求 agent「在结尾 @下一个人」,一旦在这里截断,
/// 尾部的 @ 会连同内容一起被切掉,接力链就断在第一跳(且毫无提示)。
/// 截断只在 [`report_for_chat`] 里做,且发生在 @ 解析之后。
pub fn extract_report(full_output: &str) -> String {
    let cleaned = collapse_blank_lines(full_output.trim());
    if cleaned.is_empty() {
        return "(无输出)".to_string();
    }
    cleaned
}

/// 把汇报压成群聊里好读的展示文本。若发生截断且这轮派了活,
/// 补一行说明派给了谁 —— 否则用户会看到一条以 … 结尾的汇报却不知道谁接手了。
pub fn report_for_chat(report: &str, mentions: &[String]) -> String {
    if report.chars().count() <= REPORT_MAX_CHARS {
        return report.to_string();
    }
    let mut out = truncate_chars(report, REPORT_MAX_CHARS);
    if !mentions.is_empty() {
        let who = mentions
            .iter()
            .map(|n| format!("@{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("\n↪ 已派给 {who}"));
    }
    out
}

/// 折叠连续空行为单个空行，并去掉每行尾随空白。
fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut prev_blank = false;
    for line in s.lines() {
        let t = line.trim_end();
        let blank = t.trim().is_empty();
        if blank && prev_blank {
            continue; // 跳过连续空行
        }
        out.push(t);
        prev_blank = blank;
    }
    // 去掉首尾空行
    while out.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// 从最终回复里解析出被 @ 的、在花名册中的名字（去重、忽略自 @）。
pub fn parse_mentions(report: &str, roster: &[String], self_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = report.char_indices().collect::<Vec<_>>();
    let mut i = 0;
    while i < bytes.len() {
        let (_, c) = bytes[i];
        if c == '@' {
            // 从 @ 后开始，尽量长地匹配某个 roster 名字（支持中文名）
            let rest: String = report[bytes[i].0 + c.len_utf8()..].to_string();
            if let Some(hit) = longest_roster_prefix(&rest, roster) {
                if hit != self_name && !out.contains(&hit) {
                    out.push(hit);
                }
            }
        }
        i += 1;
    }
    out
}

/// 在 rest 的开头,找一个最长的、命中花名册的名字。
fn longest_roster_prefix(rest: &str, roster: &[String]) -> Option<String> {
    let mut best: Option<String> = None;
    for name in roster {
        if rest.starts_with(name.as_str()) {
            match &best {
                Some(b) if b.chars().count() >= name.chars().count() => {}
                _ => best = Some(name.clone()),
            }
        }
    }
    best
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut r: String = s.chars().take(max).collect();
        r.push('…');
        r
    }
}

/// 剥去 ANSI 转义序列（CSI / OSC）。
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: 吃到字母结束
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: 吃到 BEL 或 ST(\x1b\\)
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if let Some('\\') = chars.peek() {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 从我输入里解析 @（用于启动开关：不带 @ 则不触发任何 agent）。
pub fn parse_owner_mentions(input: &str, roster: &[String]) -> Vec<String> {
    parse_mentions(input, roster, "我")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<String> {
        vec!["老K".into(), "阿码".into(), "小盾".into(), "阿测".into()]
    }

    #[test]
    fn extract_trims_and_keeps_body() {
        let raw = "\n  改完了 auth.py @小盾 对下接口  \n";
        assert_eq!(extract_report(raw), "改完了 auth.py @小盾 对下接口");
    }

    #[test]
    fn extract_collapses_blank_lines() {
        let raw = "第一段\n\n\n\n第二段";
        assert_eq!(extract_report(raw), "第一段\n\n第二段");
    }

    #[test]
    fn extract_empty_is_placeholder() {
        assert_eq!(extract_report("   \n\n  "), "(无输出)");
    }

    #[test]
    fn extract_does_not_truncate() {
        // 截断必须留到 @ 解析之后 —— 否则结尾的 @ 会被切掉,接力链直接断
        let raw = format!("{}\n@小盾 接手", "改完了。".repeat(200));
        let r = extract_report(&raw);
        assert!(r.chars().count() > REPORT_MAX_CHARS);
        assert!(r.ends_with("@小盾 接手"));
        // 完整文本里解析得到 @
        assert_eq!(parse_mentions(&r, &roster(), "阿码"), vec!["小盾".to_string()]);
    }

    #[test]
    fn report_for_chat_truncates_and_notes_targets() {
        let long = "改完了。".repeat(200);
        let out = report_for_chat(&long, &["小盾".to_string()]);
        assert_eq!(out.lines().next().unwrap().chars().count(), REPORT_MAX_CHARS + 1);
        assert!(out.contains('…'));
        assert!(out.ends_with("↪ 已派给 @小盾"));
    }

    #[test]
    fn report_for_chat_leaves_short_text_alone() {
        let out = report_for_chat("干完了 @小盾 对下接口", &["小盾".to_string()]);
        assert_eq!(out, "干完了 @小盾 对下接口");
    }

    #[test]
    fn mentions_basic() {
        let m = parse_mentions("①校验层抽完 @小盾 对接口 @阿测", &roster(), "阿码");
        assert_eq!(m, vec!["小盾".to_string(), "阿测".to_string()]);
    }

    #[test]
    fn mentions_ignore_self() {
        let m = parse_mentions("我 @阿码 自己不该被唤醒 @小盾", &roster(), "阿码");
        assert_eq!(m, vec!["小盾".to_string()]);
    }

    #[test]
    fn mentions_ignore_unknown() {
        let m = parse_mentions("@param 是代码 @陌生人 不在册", &roster(), "老K");
        assert!(m.is_empty());
    }

    #[test]
    fn mentions_dedup() {
        let m = parse_mentions("@小盾 再 @小盾", &roster(), "老K");
        assert_eq!(m, vec!["小盾".to_string()]);
    }

    #[test]
    fn owner_no_mention_is_empty() {
        let m = parse_owner_mentions("随便记一句备注", &roster());
        assert!(m.is_empty());
    }

    #[test]
    fn strip_ansi_works() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
