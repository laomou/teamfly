//! issue 落盘与增量前情:jsonl 追加/重放,拼装被唤醒 agent 的群聊前情。

use crate::model::{ChatMsg, Issue, Member};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// issues 目录:<teamfly_dir>/issues
pub fn issues_dir(teamfly_dir: &Path) -> PathBuf {
    teamfly_dir.join("issues")
}

fn issue_path(teamfly_dir: &Path, name: &str) -> PathBuf {
    issues_dir(teamfly_dir).join(format!("{name}.jsonl"))
}

/// 追加一条群聊消息到落盘文件。
pub fn append_chat(teamfly_dir: &Path, issue_name: &str, msg: &ChatMsg) -> Result<()> {
    let dir = issues_dir(teamfly_dir);
    std::fs::create_dir_all(&dir)?;
    let path = issue_path(teamfly_dir, issue_name);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开 {}", path.display()))?;
    let line = serde_json::to_string(msg)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// 从盘上重放所有 issue(重开恢复 tab 与时间线)。
pub fn load_all_issues(teamfly_dir: &Path) -> Result<Vec<Issue>> {
    let dir = issues_dir(teamfly_dir);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();

    let mut issues = Vec::new();
    for path in files {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("issue")
            .to_string();
        let content = std::fs::read_to_string(&path)?;
        let mut issue = Issue::new(name);
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMsg>(line) {
                issue.timeline.push(msg);
            }
        }
        issues.push(issue);
    }
    Ok(issues)
}

/// 为被唤醒的 agent 拼「自上次活跃以来新增的群聊前情」+ 本次指派。
///
/// - `timeline`:当前 issue 的完整群聊时间线
/// - `member`:被唤醒者(读它的 last_seen_chat_len 算增量)
/// - `assignment`:本次投递给它的指派原文(来自 @ 或我)
pub fn build_prompt_input(timeline: &[ChatMsg], member: &Member, assignment: &str) -> String {
    let start = member.last_seen_chat_len.min(timeline.len());
    let recent = &timeline[start..];

    let mut s = String::new();
    if !recent.is_empty() {
        s.push_str("[群聊新进展]\n");
        for m in recent {
            let who = if m.is_system { "系统" } else { &m.author };
            s.push_str(&format!("{who}: {}\n", m.text));
        }
        s.push_str("---\n");
    }
    s.push_str("现在轮到你:\n");
    s.push_str(assignment);
    s.push_str(&format!(
        "\n\n（干完后,请用一行以「{}」开头的话向群里汇报结论,需要谁接力就 @他。）",
        crate::router::CHAT_MARK
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentState, BackendKind};
    use std::collections::VecDeque;

    fn member(seen: usize) -> Member {
        Member {
            name: "小盾".into(),
            role: "安全".into(),
            emoji: "🛡".into(),
            backend: BackendKind::Claude,
            model: None,
            mcp_config: None,
            system_prompt: String::new(),
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            raw: VecDeque::new(),
            last_seen_chat_len: seen,
        }
    }

    fn msg(author: &str, text: &str) -> ChatMsg {
        ChatMsg {
            ts: "t".into(),
            author: author.into(),
            text: text.into(),
            is_system: false,
        }
    }

    #[test]
    fn incremental_context() {
        let tl = vec![
            msg("我", "把 auth 抽出来"),
            msg("老K", "拆三块 @小盾 接②"),
        ];
        let input = build_prompt_input(&tl, &member(1), "[来自 老K] 接②限流");
        // 只应含第 2 条(增量),不含第 1 条
        assert!(input.contains("拆三块"));
        assert!(!input.contains("把 auth 抽出来"));
        assert!(input.contains("接②限流"));
        assert!(input.contains("【群聊】"));
    }

    #[test]
    fn no_recent_when_caught_up() {
        let tl = vec![msg("我", "x")];
        let input = build_prompt_input(&tl, &member(1), "干活");
        assert!(!input.contains("[群聊新进展]"));
    }
}
