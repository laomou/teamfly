//! 路由：从 agent 的 raw 输出提取【群聊】汇报，解析 @ 指派，防乒乓。
//! 尽量纯函数，便于单测。

/// 群聊汇报标记。
pub const CHAT_MARK: &str = "【群聊】";

/// 从整轮 raw 输出里提取「面向群聊的一句汇报」。
/// 优先取以【群聊】开头的行；没有则兜底取最后一段非空文本。
pub fn extract_report(full_output: &str) -> String {
    // 1) 找【群聊】行（可能多行，取最后一条，通常是收尾汇报）
    let marked: Vec<&str> = full_output
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.find(CHAT_MARK).map(|i| t[i + CHAT_MARK.len()..].trim())
        })
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(last) = marked.last() {
        return last.to_string();
    }
    // 2) 兜底：取最后一段非空行（截断过长）
    let last_line = full_output
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("(无输出)");
    let s = last_line.to_string();
    truncate_chars(&s, 200)
}

/// 从一条汇报文本里解析出被 @ 的、在花名册中的名字（去重、忽略自 @）。
/// 只在汇报行内解析 —— 调用方保证传入的是汇报文本，而非 raw 正文。
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
    fn extract_marked_report() {
        let raw = "读了 auth.py\n想了想\n【群聊】①校验层抽完 @小盾 对接口";
        assert_eq!(extract_report(raw), "①校验层抽完 @小盾 对接口");
    }

    #[test]
    fn extract_fallback_last_line() {
        let raw = "开始干活\n改完了 auth.py";
        assert_eq!(extract_report(raw), "改完了 auth.py");
    }

    #[test]
    fn extract_takes_last_marked() {
        let raw = "【群聊】阶段一好了\n又干了点\n【群聊】全好了 @老K";
        assert_eq!(extract_report(raw), "全好了 @老K");
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
