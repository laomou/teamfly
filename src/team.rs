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
    /// 指定模型(可选)。不写则由 backend 的 CLI 自己决定
    /// (claude 读 ANTHROPIC_MODEL,codex 读它自己的 config)。
    #[serde(default)]
    model: Option<String>,
    /// 只读成员(主工作目录 + 无写权限),适合评审/调度。默认 false = 可写。
    #[serde(default)]
    read_only: bool,
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
        bail!("agents/ 里没有任何 .md 成员定义");
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
            read_only: af.read_only,
            system_prompt: sp,
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            working_issue: None,
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
                    "重名成员: {} 与 {}(@ 匹配不区分 ASCII 大小写)",
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

    /// md 里写了 `model:` 就必须真的被解析出来 —— 该字段曾被删掉一轮,
    /// 期间内置 md 里那几行是**静默失效**的(serde 对未知字段不报错,配了也白配)。
    ///
    /// 只校验「写了的能读出来」,不要求每个成员都写:`model` 是可选的,
    /// 不填就跟随 CLI 自己的默认。断言全员 Some 会把「当前恰好都配了」
    /// 这个偶然事实变成硬约束,以后想让某个 agent 不指定模型就会误报。
    #[test]
    fn model_in_md_is_parsed_and_optional() {
        let dir = std::env::temp_dir().join(format!("tf_mdl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::builtin::seed_default(&dir).unwrap();
        let team_dir = dir.join("teams").join(crate::builtin::DEFAULT_TEAM);

        // md 里写了 model: 的成员名 → 期望值,直接从磁盘上的 md 读
        let mut want: std::collections::HashMap<String, String> = Default::default();
        for e in std::fs::read_dir(team_dir.join("agents")).unwrap().flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap();
            let front: Vec<&str> = text.splitn(3, "---").collect();
            let Some(fm) = front.get(1) else { continue };
            let get = |k: &str| {
                fm.lines()
                    .find_map(|l| l.trim().strip_prefix(&format!("{k}:")))
                    .map(|v| v.trim().to_string())
            };
            if let (Some(n), Some(mdl)) = (get("name"), get("model")) {
                want.insert(n, mdl);
            }
        }
        assert!(!want.is_empty(), "内置 md 里一个 model: 都没有,这个测试就白测了");

        let team = load_team(&team_dir).unwrap();
        for m in &team.members {
            assert_eq!(
                m.model.as_deref(),
                want.get(&m.name).map(|s| s.as_str()),
                "{} 的 model 解析结果和 md 里写的不一致",
                m.name
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不写 `model:` 的成员解析成 None —— 这是「跟随 CLI 默认」的入口,
    /// 和写了的成员可以在同一个团队里混用。
    #[test]
    fn member_without_model_is_none() {
        let dir = std::env::temp_dir().join(format!("tf_nomdl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let agents = dir.join("teams").join("t").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(dir.join("teams").join("t").join("team.md"), "name: 测试队\n").unwrap();
        std::fs::write(agents.join("有.md"), "---\nname: 有\nbackend: claude\nmodel: m-1\n---\n人设\n").unwrap();
        std::fs::write(agents.join("无.md"), "---\nname: 无\nbackend: claude\n---\n人设\n").unwrap();

        let team = load_team(&dir.join("teams").join("t")).unwrap();
        let by = |n: &str| team.members.iter().find(|m| m.name == n).expect(n);
        assert_eq!(by("有").model.as_deref(), Some("m-1"));
        assert_eq!(by("无").model, None, "没写 model: 的该是 None,不能凭空造一个");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_frontmatter_basic() {
        let c = "---\nname: 老K\nbackend: mock\n---\n你是老K";
        let (f, b) = split_frontmatter(c);
        assert!(f.contains("name: 老K"));
        assert_eq!(b, "你是老K");
    }

    /// read_only 字段:显式 true / 显式 false / 省略(默认 false = 可写)。
    ///
    /// 默认必须是**可写** —— 漏写这个字段的成员是普通干活的成员,
    /// 不该被静默降级成只读(那样它改不了文件,而汇报里看起来像正常干完了)。
    /// 想要只读得显式声明。
    #[test]
    fn read_only_field_defaults_to_writable() {
        let parse = |front: &str| -> bool {
            serde_yaml::from_str::<AgentFront>(front).unwrap().read_only
        };
        assert!(parse("name: REV\nbackend: claude\nread_only: true"));
        assert!(!parse("name: DEV\nbackend: claude\nread_only: false"));
        assert!(!parse("name: DEV\nbackend: claude"), "省略时必须默认可写");
    }

    #[test]
    fn split_frontmatter_none() {
        let (f, b) = split_frontmatter("just body");
        assert!(f.is_empty());
        assert_eq!(b, "just body");
    }
}
