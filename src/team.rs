//! 团队加载:解析 team.md 与 agents/*.md,拼装每个 agent 的 system prompt,启动预检。

use crate::model::{AgentState, BackendKind, Member};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;

/// 单个 agent md 的 frontmatter。
#[derive(Debug, serde::Deserialize)]
struct AgentFront {
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    emoji: Option<String>,
    backend: String,
    #[serde(default)]
    model: Option<String>,
}

/// team.md 的 frontmatter。
#[derive(Debug, serde::Deserialize, Default)]
struct TeamFront {
    #[serde(default)]
    name: Option<String>,
}

pub struct Team {
    pub name: String,
    pub members: Vec<Member>,
}

/// 把 md 文件切成 (frontmatter_yaml, body)。
fn split_frontmatter(content: &str) -> (String, String) {
    let trimmed = content.trim_start_matches('\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---") {
        // 找下一个 --- 行
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let body_start = end + "\n---".len();
            let body = rest[body_start..]
                .trim_start_matches(['\r', '\n'])
                .to_string();
            // front 可能以换行开头
            return (front.trim_start_matches(['\r', '\n']).to_string(), body);
        }
    }
    (String::new(), content.to_string())
}

fn parse_backend(s: &str) -> Result<BackendKind> {
    match s.trim().to_lowercase().as_str() {
        "claude" => Ok(BackendKind::Claude),
        "codex" => Ok(BackendKind::Codex),
        other => bail!("未知 backend: {other}(应为 claude/codex)"),
    }
}

/// 根据 role 猜一个默认 emoji（可被 frontmatter 覆盖）。
fn default_emoji(role: &str) -> &'static str {
    match role {
        r if r.contains("架构") => "🧭",
        r if r.contains("实现") || r.contains("开发") => "💻",
        r if r.contains("安全") || r.contains("评审") => "🛡",
        r if r.contains("测试") => "🧪",
        _ => "👤",
    }
}

/// 加载团队文件夹。
pub fn load_team(dir: &Path) -> Result<Team> {
    if !dir.is_dir() {
        bail!("团队文件夹不存在: {}", dir.display());
    }

    // team.md（可选）
    let team_md = dir.join("team.md");
    let (team_name, common_prompt) = if team_md.exists() {
        let content = std::fs::read_to_string(&team_md)
            .with_context(|| format!("读取 {}", team_md.display()))?;
        let (front, body) = split_frontmatter(&content);
        let tf: TeamFront = if front.trim().is_empty() {
            TeamFront::default()
        } else {
            serde_yaml::from_str(&front).with_context(|| "解析 team.md frontmatter")?
        };
        (tf.name, body)
    } else {
        (None, String::new())
    };

    // agents/*.md
    let agents_dir = dir.join("agents");
    if !agents_dir.is_dir() {
        bail!("缺少 agents/ 目录: {}", agents_dir.display());
    }
    let mut entries: Vec<_> = std::fs::read_dir(&agents_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    entries.sort();

    if entries.is_empty() {
        bail!("agents/ 里没有任何 .md 群友定义");
    }

    let mut members = Vec::new();
    for path in entries {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 {}", path.display()))?;
        let (front, body) = split_frontmatter(&content);
        if front.trim().is_empty() {
            bail!("{} 缺少 frontmatter", path.display());
        }
        let af: AgentFront = serde_yaml::from_str(&front)
            .with_context(|| format!("解析 {} frontmatter", path.display()))?;

        let backend = parse_backend(&af.backend)
            .with_context(|| format!("{} 的 backend", path.display()))?;

        // 拼装最终 system prompt = 公共 + 个人
        let mut sp = String::new();
        if !common_prompt.trim().is_empty() {
            sp.push_str(common_prompt.trim());
            sp.push_str("\n\n");
        }
        sp.push_str(body.trim());

        let emoji = af
            .emoji
            .unwrap_or_else(|| default_emoji(&af.role).to_string());

        members.push(Member {
            name: af.name,
            role: af.role,
            emoji,
            backend,
            model: af.model,
            system_prompt: sp,
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            raw: VecDeque::new(),
            last_seen: std::collections::HashMap::new(),
        });
    }

    // 名字唯一性。**大小写不敏感**地比:@ 匹配对 ASCII 名字是大小写不敏感的,
    // 所以 `DEV` 和 `dev` 同时在册时 `@dev` 永远只命中靠前那个,
    // 另一个成员永久无法被 @ 且没有任何告警。
    for i in 0..members.len() {
        for j in (i + 1)..members.len() {
            if members[i].name.eq_ignore_ascii_case(&members[j].name) {
                bail!(
                    "重名群友: {} 与 {}(@ 匹配不区分 ASCII 大小写)",
                    members[i].name,
                    members[j].name
                );
            }
        }
    }
    // 名字必须能被 @ 出来
    for m in &members {
        if m.name.trim().is_empty() {
            bail!("有成员的 name 是空的");
        }
        if m.name.chars().any(|c| c.is_whitespace() || c == '@') {
            bail!(
                "成员名不能含空白或 @:{:?}(否则 @ 派活匹配不到它)",
                m.name
            );
        }
    }

    let name = team_name
        .or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| anyhow!("无法确定团队名"))?;

    Ok(Team { name, members })
}

/// 启动预检:校验 backend 所需的 CLI / 凭证是否就绪。返回警告列表(不致命的作为提示)。
pub fn preflight(team: &Team) -> Vec<String> {
    let mut warns = Vec::new();
    let mut need_claude = false;
    let mut need_codex = false;
    for m in &team.members {
        match m.backend {
            BackendKind::Claude => need_claude = true,
            BackendKind::Codex => need_codex = true,
        }
    }
    if need_claude && which("claude").is_none() {
        warns.push("有成员用 claude backend,但 PATH 里找不到 `claude`".into());
    }
    if need_codex && which("codex").is_none() {
        warns.push("有成员用 codex backend,但 PATH 里找不到 `codex`".into());
    }
    warns
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_basic() {
        let c = "---\nname: 老K\nbackend: mock\n---\n你是老K";
        let (f, b) = split_frontmatter(c);
        assert!(f.contains("name: 老K"));
        assert_eq!(b, "你是老K");
    }

    #[test]
    fn split_frontmatter_none() {
        let (f, b) = split_frontmatter("just body");
        assert!(f.is_empty());
        assert_eq!(b, "just body");
    }
}
