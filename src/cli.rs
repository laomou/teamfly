//! CLI:teamfly work [工作目录] [--team <团队名字>] / teamfly init

use crate::model::{Issue, Model, Selection};
use crate::team;
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "teamfly", about = "终端里的 AI 团队协作台 —— 你带一个群,agent 是群友")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// 用某团队在工作目录里开群
    Work {
        /// 工作目录(缺省 = 当前目录)
        dir: Option<PathBuf>,
        /// 团队名字(.teamfly/teams/<名> 下的子目录名)
        #[arg(long)]
        team: Option<String>,
    },
    /// 交互式配置用户级 ~/.teamfly/env.toml(输入 BASE_URL / KEY)
    Init,
}

/// 交互式初始化用户级配置:问 claude/codex 的 BASE_URL 和 KEY,写 ~/.teamfly/env.toml。
pub fn init() -> Result<()> {
    use std::io::Write;

    let path = crate::env::user_env_path()
        .ok_or_else(|| anyhow::anyhow!("无法确定 ~/.teamfly 路径(HOME 未设?)"))?;

    println!("配置 teamfly 用户级环境变量 → {}", path.display());
    println!("(直接回车跳过某项;已有文件会被覆盖)\n");

    let prompt = |label: &str, default: &str| -> Result<String> {
        if default.is_empty() {
            print!("{label}: ");
        } else {
            print!("{label} [{default}]: ");
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let v = line.trim().to_string();
        Ok(if v.is_empty() { default.to_string() } else { v })
    };

    println!("── claude backend ──");
    let anthropic_base = prompt("ANTHROPIC_BASE_URL", "https://api.anthropic.com")?;
    let anthropic_token = prompt("ANTHROPIC_AUTH_TOKEN", "")?;

    println!("\n── codex backend(可跳过)──");
    let openai_base = prompt("OPENAI_BASE_URL", "")?;
    let openai_key = prompt("OPENAI_API_KEY", "")?;

    // 组装 toml
    let mut out = String::new();
    out.push_str("# teamfly 用户级 agent 环境变量(teamfly init 生成)\n");
    out.push_str("# 项目里可写 <工作目录>/.teamfly/env.toml 覆盖同名 key\n\n");

    out.push_str("[claude]\n");
    if !anthropic_base.is_empty() {
        out.push_str(&format!("ANTHROPIC_BASE_URL   = \"{anthropic_base}\"\n"));
    }
    if !anthropic_token.is_empty() {
        out.push_str(&format!("ANTHROPIC_AUTH_TOKEN = \"{anthropic_token}\"\n"));
    }

    if !openai_base.is_empty() || !openai_key.is_empty() {
        out.push_str("\n[codex]\n");
        if !openai_base.is_empty() {
            out.push_str(&format!("OPENAI_BASE_URL = \"{openai_base}\"\n"));
        }
        if !openai_key.is_empty() {
            out.push_str(&format!("OPENAI_API_KEY  = \"{openai_key}\"\n"));
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, out)?;
    // 权限收紧(含 key)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("\n✓ 已写入 {}", path.display());

    // 顺手建 ~/.teamfly/mcp.json 骨架(不存在才建)
    if let Some(mcp) = crate::env::user_mcp_path() {
        if crate::env::seed_user_mcp(&mcp).unwrap_or(false) {
            println!("✓ 已建 MCP 骨架 {}(需要接 MCP 时在这里加)", mcp.display());
        }
    }
    Ok(())
}

/// 组装初始 Model。
pub fn build(dir: Option<PathBuf>, team_arg: Option<String>) -> Result<(Model, Vec<String>)> {
    let work_dir = dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));

    let teamfly_dir = work_dir.join(".teamfly");
    std::fs::create_dir_all(&teamfly_dir)?;

    // 首次运行:播种内置默认团队到 .teamfly/teams/default(已存在则不动)
    crate::builtin::seed_default(&teamfly_dir)?;

    // 加载 agent 环境变量(.teamfly/env.toml,可选)
    let agent_env = crate::env::load(&teamfly_dir)?;

    // 团队来源优先级:--team > 唯一/default 团队
    let team_dir = resolve_team_dir(team_arg, &teamfly_dir)?;
    let team = team::load_team(&team_dir)?;
    let mut warns = team::preflight(&team);
    // env.toml 里未展开的 ${VAR}
    for name in &agent_env.unresolved {
        warns.push(format!("env.toml 里 ${{{name}}} 未定义,将按字面量传给 agent"));
    }

    // 恢复落盘的议题;没有则建一个默认议题
    let mut issues = crate::issue::load_all_issues(&teamfly_dir)?;
    let fresh_start = issues.is_empty();
    if fresh_start {
        issues.push(Issue::new("默认议题"));
    }

    // 首次开箱:塞一条欢迎消息进默认议题(不落盘,仅当前会话)
    if fresh_start {
        let member_hints: Vec<String> = team
            .members
            .iter()
            .map(|m| format!("@{}", m.name))
            .collect();
        let welcome = format!(
            "欢迎!输入框打字发言,带 @名字 才会派活。 成员:{} · 按 ? 看帮助",
            member_hints.join(" · ")
        );
        issues[0].timeline.push(crate::model::ChatMsg {
            ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            author: "系统".into(),
            text: welcome,
            is_system: true,
        });
    }

    let model = Model {
        team_name: team.name,
        work_dir,
        teamfly_dir,
        agent_env,
        members: team.members,
        issues,
        current_issue: 0,
        selection: Selection::Chat,
        input_mode: crate::model::InputMode::Chat,
        input: String::new(),
        scroll: 0,
        tick: 0,
        should_quit: false,
        max_chain_depth: 12,
        status_hint: None,
        status_hint_until: 0,
        pending_delete: None,
        show_help: false,
    };

    Ok((model, warns))
}

fn resolve_team_dir(team_arg: Option<String>, teamfly_dir: &std::path::Path) -> Result<PathBuf> {
    let teams_dir = teamfly_dir.join("teams");

    if let Some(name) = team_arg {
        // --team 是团队名字:.teamfly/teams/<名>
        let by_name = teams_dir.join(&name);
        if by_name.is_dir() {
            return Ok(by_name);
        }
        bail!(
            "找不到团队「{}」。它应是 {} 下的一个子目录名。\n现有团队:{}",
            name,
            teams_dir.display(),
            list_team_names(&teams_dir)
        );
    }

    // 不指定 --team:default 优先;否则唯一团队则用它,否则要求指定
    if teams_dir.is_dir() {
        let names = team_names(&teams_dir);
        let default_dir = teams_dir.join(crate::builtin::DEFAULT_TEAM);
        if default_dir.is_dir() {
            return Ok(default_dir);
        }
        if names.len() == 1 {
            return Ok(teams_dir.join(&names[0]));
        }
        if !names.is_empty() {
            bail!(
                "有多个团队,请用 --team <名字> 指定其一:{}",
                names.join(" · ")
            );
        }
    }
    bail!(
        "没有团队。请在 {} 下放一个团队文件夹(team.md + agents/*.md),\n\
         或用 --team <名字> 指定。",
        teams_dir.display()
    )
}

/// 列出 teams 目录下的团队名(子目录)。
fn team_names(teams_dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(teams_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

fn list_team_names(teams_dir: &std::path::Path) -> String {
    let names = team_names(teams_dir);
    if names.is_empty() {
        "(空)".to_string()
    } else {
        names.join(" · ")
    }
}
