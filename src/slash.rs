//! 斜杠命令解析。
//!
//! - `/init`   → macro:展开为一句给 DEV 的话,走正常 @ 派活
//! - `/team X` → 命令:热切当前议题的团队(换 members,议题保留)
//!
//! 未知斜杠 = 无效,由调用方给出提示。

/// 解析结果。
#[derive(Debug, PartialEq)]
pub enum Slash {
    /// 展开为一句话,像我发的普通消息一样处理(会 @ 派活)。
    Macro { expanded: String },
    /// 热切当前议题的团队。
    SwitchTeam { name: String },
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
        "/init" => Some(Slash::Macro {
            expanded: init_macro().to_string(),
        }),
        "/team" => {
            if args.is_empty() {
                Some(Slash::Unknown { text: s.to_string() })
            } else {
                Some(Slash::SwitchTeam { name: args.to_string() })
            }
        }
        _ => Some(Slash::Unknown { text: s.to_string() }),
    }
}

/// /init 展开的目标文本。会作为「我」的消息进时间线并派活给 @DEV。
fn init_macro() -> &'static str {
    "@DEV 请扫描当前工作目录:识别项目类型、关键目录/文件、常用命令、开发工作流。\
     产出一份简洁的 CLAUDE.md(如已存在则更新),内容包括:项目一句话描述、目录结构、\
     常用命令(build/test/run)、注意事项。汇报时用【群聊】说清楚做了什么。"
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
    fn init_expands_to_dev_prompt() {
        match parse("/init") {
            Some(Slash::Macro { expanded }) => {
                assert!(expanded.contains("@DEV"));
                assert!(expanded.contains("CLAUDE.md"));
            }
            other => panic!("expected Macro, got {other:?}"),
        }
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
