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
///
/// 会**读取现有文件里的值当默认值**。以前是无条件全量重写,用户手写的
/// `[codex]` 段、ANTHROPIC_MODEL 之类会在再跑一次 init 后静默消失,
/// 而提示语「直接回车跳过」又让人以为跳过 = 保留原值。
pub fn init() -> Result<()> {
    let path = crate::env::user_env_path()
        .ok_or_else(|| anyhow::anyhow!("无法确定 ~/.teamfly 路径(HOME 未设?)"))?;

    println!("配置 teamfly 用户级环境变量 → {}", path.display());

    // 读现有配置作默认值
    let existing = read_existing_env(&path);
    if !existing.is_empty() {
        println!("(已读到现有配置,回车即保留方括号里的原值)");
    }
    println!("(密钥输入不回显;回车跳过某项)\n");

    let cur = |section: &str, key: &str| -> String {
        existing
            .get(&format!("{section}.{key}"))
            .cloned()
            .unwrap_or_default()
    };

    println!("── claude backend ──");
    let anthropic_base = prompt_line(
        "ANTHROPIC_BASE_URL",
        &or_default(cur("claude", "ANTHROPIC_BASE_URL"), "https://api.anthropic.com"),
    )?;
    let anthropic_token = prompt_secret("ANTHROPIC_AUTH_TOKEN", &cur("claude", "ANTHROPIC_AUTH_TOKEN"))?;

    println!("\n── codex backend(可跳过)──");
    let openai_base = prompt_line("OPENAI_BASE_URL", &cur("codex", "OPENAI_BASE_URL"))?;
    let openai_key = prompt_secret("OPENAI_API_KEY", &cur("codex", "OPENAI_API_KEY"))?;

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
    // 保留现有 [claude] 段里 init 不管的其它 key(如 ANTHROPIC_MODEL)
    for (k, v) in extra_keys(&existing, "claude", &["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]) {
        out.push_str(&format!("{k} = \"{v}\"\n"));
    }

    let codex_extra = extra_keys(&existing, "codex", &["OPENAI_BASE_URL", "OPENAI_API_KEY"]);
    if !openai_base.is_empty() || !openai_key.is_empty() || !codex_extra.is_empty() {
        out.push_str("\n[codex]\n");
        if !openai_base.is_empty() {
            out.push_str(&format!("OPENAI_BASE_URL = \"{openai_base}\"\n"));
        }
        if !openai_key.is_empty() {
            out.push_str(&format!("OPENAI_API_KEY  = \"{openai_key}\"\n"));
        }
        for (k, v) in codex_extra {
            out.push_str(&format!("{k} = \"{v}\"\n"));
        }
    }
    // 顶层(不分段)的 key 也留着
    let top_extra = extra_keys(&existing, "", &[]);
    if !top_extra.is_empty() {
        out.push_str("\n# —— 原有的顶层 key(所有 backend 共用)——\n");
        for (k, v) in top_extra {
            out.push_str(&format!("{k} = \"{v}\"\n"));
        }
    }

    // 原子 + 0600 写入(不再「先 0644 再 chmod」留可读窗口)
    crate::env::write_private(&path, &out)?;
    println!("\n✓ 已写入 {}(权限 0600)", path.display());

    // 顺手建 ~/.teamfly/mcp.json 骨架(不存在才建)
    if let Some(mcp) = crate::env::user_mcp_path() {
        if crate::env::seed_user_mcp(&mcp).unwrap_or(false) {
            println!("✓ 已建 MCP 骨架 {}(需要接 MCP 时在这里加)", mcp.display());
        }
    }
    Ok(())
}

fn or_default(v: String, fallback: &str) -> String {
    if v.is_empty() { fallback.to_string() } else { v }
}

/// 读现有 env.toml,拉平成 "段.key" → 值(顶层 key 的段名为空串)。
/// 解析失败就当没有 —— 但要明说,别让用户以为原值被保留了。
fn read_existing_env(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            println!("⚠ 现有 {} 解析失败({e});这次将按空配置重写,原文件内容会丢失。", path.display());
            return out;
        }
    };
    for (k, v) in table {
        match v {
            toml::Value::Table(sub) => {
                for (sk, sv) in sub {
                    if let Some(s) = scalar(&sv) {
                        out.insert(format!("{k}.{sk}"), s);
                    }
                }
            }
            other => {
                if let Some(s) = scalar(&other) {
                    out.insert(format!(".{k}"), s);
                }
            }
        }
    }
    out
}

fn scalar(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// 某段里除 `known` 之外的 key —— init 不认识但用户写了的,要原样留下。
fn extra_keys(
    existing: &std::collections::BTreeMap<String, String>,
    section: &str,
    known: &[&str],
) -> Vec<(String, String)> {
    let prefix = format!("{section}.");
    existing
        .iter()
        .filter_map(|(k, v)| {
            let rest = k.strip_prefix(&prefix)?;
            if rest.contains('.') || known.contains(&rest) {
                None
            } else {
                Some((rest.to_string(), v.clone()))
            }
        })
        .collect()
}

fn prompt_line(label: &str, default: &str) -> Result<String> {
    prompt_line_inner(label, default, false)
}

/// 按行读,但**不回显默认值** —— 用于密钥的非交互回退路径。
/// 直接把旧 key 打在提示里等于又泄露一次(会进 stdout / 日志 / CI 输出)。
fn prompt_line_secret(label: &str, default: &str) -> Result<String> {
    prompt_line_inner(label, default, true)
}

fn prompt_line_inner(label: &str, default: &str, hide_default: bool) -> Result<String> {
    use std::io::Write;
    if default.is_empty() {
        print!("{label}: ");
    } else if hide_default {
        print!("{label} [已有值,回车保留]: ");
    } else {
        print!("{label} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let v = line.trim().to_string();
    Ok(if v.is_empty() { default.to_string() } else { v })
}

/// 读一个密钥:不回显。
///
/// 以前用普通 read_line,key 会原样打在屏幕上并留在 scrollback 里 ——
/// 在 tmux/screen 里跑(这是个 TUI 工具,很常见)就进了它的缓冲区,
/// 开了 pipe-pane / script / asciinema 录制的话直接落进明文日志。
fn prompt_secret(label: &str, default: &str) -> Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use std::io::{IsTerminal, Write};

    // 非交互(管道/脚本喂 stdin):既没法关回显,也不存在 scrollback 泄露,
    // 退回按行读。否则 raw mode 会直接报 os error 6,整个 init 都跑不起来。
    if !std::io::stdin().is_terminal() {
        return prompt_line_secret(label, default);
    }

    let masked = if default.is_empty() {
        String::new()
    } else {
        " [已有值,回车保留]".to_string()
    };
    print!("{label}{masked}: ");
    std::io::stdout().flush()?;

    // raw mode 起不来(比如被重定向)也退回按行读,别让 init 整个失败
    if crossterm::terminal::enable_raw_mode().is_err() {
        return prompt_line_secret(label, default);
    }
    let mut buf = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(k)) => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("已取消"));
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.clear();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    crossterm::terminal::disable_raw_mode()?; // 无论成败都恢复,别把终端留在 raw
    println!();
    result?;

    let v = buf.trim().to_string();
    Ok(if v.is_empty() { default.to_string() } else { v })
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
        scroll_max: std::cell::Cell::new(0),
        tick: 0,
        should_quit: false,
        // TPM 星型调度:用户→TPM→DEV→TPM→REV→TPM→… 一个需求轻松 6-8 跳,
        // 复审来回还会更长,给足余量避免被防乒乓误暂停。
        max_chain_depth: 24,
        status_hint: None,
        status_hint_until: 0,
        pending_delete: None,
        show_help: false,
        cancel: tokio_util::sync::CancellationToken::new(),
        team_gen: 0,
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
