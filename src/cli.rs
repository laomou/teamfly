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

    // 在**现有 toml 树上原地改**这四个 key,再整棵序列化回去。
    //
    // 以前是手拼字符串 `K = "{v}"`:值里含 `"` 或 `\` 就写出非法 TOML,
    // 下次 teamfly 在 env::load 就 bail,TUI 根本进不去;而且只认识
    // claude/codex/顶层标量三处,别的段、嵌套表、数组一律被静默删掉。
    // 交给 toml crate 序列化则转义和结构都不用自己操心。
    let mut table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => toml::Table::new(),
    };
    set_or_remove(&mut table, "claude", "ANTHROPIC_BASE_URL", &anthropic_base);
    set_or_remove(&mut table, "claude", "ANTHROPIC_AUTH_TOKEN", &anthropic_token);
    set_or_remove(&mut table, "codex", "OPENAI_BASE_URL", &openai_base);
    set_or_remove(&mut table, "codex", "OPENAI_API_KEY", &openai_key);

    // 清掉空段(比如 codex 全跳过时),免得留一行光秃秃的 [codex]
    table.retain(|_, v| !matches!(v.as_table(), Some(t) if t.is_empty()));

    let body = toml::to_string_pretty(&table)?;
    let out = format!(
        "# teamfly 用户级 agent 环境变量(teamfly init 生成/更新)\n\
         # 注意:项目里若有 <工作目录>/.teamfly/env.toml,它会**整体顶替**本文件,\n\
         # 不是逐 key 覆盖 —— 项目级里没写的 key(比如 token)不会从这里继承。\n\n{body}"
    );

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

/// 往 `[section]` 里写一个 key;值为空则删掉这个 key(而不是写空串)。
/// 段不存在会新建;段存在但不是表(用户写歪了)则不动,免得把它的内容冲掉。
fn set_or_remove(table: &mut toml::Table, section: &str, key: &str, value: &str) {
    let entry = table
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(sub) = entry.as_table_mut() else { return };
    if value.is_empty() {
        sub.remove(key);
    } else {
        sub.insert(key.to_string(), toml::Value::String(value.to_string()));
    }
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
    let (mut issues, issue_warns) = crate::issue::load_all_issues(&teamfly_dir)?;
    warns.extend(issue_warns);
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
            "欢迎!@名字 派活,不带 @ 只是留言。成员:{} · ? 帮助",
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
        input: String::new(),
        scroll: 0,
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

    // 多实例检测:写一个 pid 文件,若已有别的活进程在用,警告(不阻止)
    if let Some(warn) = check_instance_lock(&model.teamfly_dir) {
        warns.push(warn);
    }

    Ok((model, warns))
}

/// 写 `.teamfly/teamfly.lock`(内容是 pid),返回 Some(warn) 若已有活进程持锁。
/// 崩溃留下的陈旧锁(pid 不存在)会被直接覆盖,不会挡住用户。
fn check_instance_lock(teamfly_dir: &std::path::Path) -> Option<String> {
    let lock = teamfly_dir.join("teamfly.lock");
    // 检查已有锁
    if let Ok(content) = std::fs::read_to_string(&lock) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            // pid 还活着?
            let alive = std::path::Path::new(&format!("/proc/{pid}")).is_dir();
            if alive && pid != std::process::id() {
                // 写入自己的 pid(追加者模式:两边都知道对方在)
                let _ = std::fs::write(&lock, format!("{}", std::process::id()));
                return Some(format!(
                    "另一个 teamfly (pid {pid}) 正在用这个目录,两边的议题历史可能互相覆盖,建议只开一个"
                ));
            }
        }
    }
    // 写入自己的 pid
    let _ = std::fs::write(&lock, format!("{}", std::process::id()));
    None
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
