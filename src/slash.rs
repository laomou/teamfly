//! 斜杠命令解析。
//!
//! - `/team X` → 命令:热切当前议题的团队(换 members,议题保留)
//!
//! 未知斜杠 = 无效,由调用方给出提示。

/// 解析结果。
#[derive(Debug, PartialEq)]
pub enum Slash {
    /// 热切当前议题的团队。
    SwitchTeam { name: String },
    /// 丢弃某个 agent 在当前议题的 worktree + 分支。
    Drop { name: String },
    /// 未知斜杠,text 是原始输入。
    Unknown { text: String },
}

/// 若 input 以 `/` 开头则解析,返回 Some;否则返回 None(表示不是斜杠)。
pub fn parse(input: &str) -> Option<Slash> {
    let s = input.trim();
    if !s.starts_with('/') {
        return None;
    }
    let mut parts = s.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    match cmd {
        "/team" => {
            if args.is_empty() {
                Some(Slash::Unknown { text: s.to_string() })
            } else {
                Some(Slash::SwitchTeam { name: args.to_string() })
            }
        }
        "/drop" => {
            if args.is_empty() {
                Some(Slash::Unknown { text: s.to_string() })
            } else {
                Some(Slash::Drop { name: args.to_string() })
            }
        }
        _ => Some(Slash::Unknown { text: s.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_slash() {
        assert!(parse("hello").is_none());
        assert!(parse("").is_none());
        assert!(parse(" not slash").is_none());
    }

    #[test]
    fn team_with_arg() {
        assert_eq!(
            parse("/team backend"),
            Some(Slash::SwitchTeam { name: "backend".into() })
        );
    }

    #[test]
    fn team_without_arg_is_unknown() {
        match parse("/team") {
            Some(Slash::Unknown { .. }) => (),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_slash() {
        match parse("/nope 123") {
            Some(Slash::Unknown { text }) => assert_eq!(text, "/nope 123"),
            other => panic!("got {other:?}"),
        }
    }
}
