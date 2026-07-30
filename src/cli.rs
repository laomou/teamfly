//! CLI:teamfly work [工作目录] [--team <团队名字>] —— 组装初始 Model。

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
}

/// 组装初始 Model。
pub fn build(dir: Option<PathBuf>, team_arg: Option<String>) -> Result<(Model, Vec<String>)> {
    let work_dir = dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let work_dir = match work_dir.canonicalize() {
        Ok(d) if d.is_dir() => d,
        Ok(d) => bail!("不是目录: {}", d.display()),
        Err(e) => bail!("路径无效: {} ({e})", work_dir.display()),
    };

    let teamfly_dir = work_dir.join(".teamfly");
    std::fs::create_dir_all(&teamfly_dir)?;

    // 首次运行:播种内置默认团队到 .teamfly/teams/default(已存在则不动)
    crate::builtin::seed_default(&teamfly_dir)?;

    // 团队来源优先级:--team > 唯一/default 团队
    let team_dir = resolve_team_dir(team_arg, &teamfly_dir)?;
    let team = team::load_team(&team_dir)?;
    let mut warns = team::preflight(&team);

    // 恢复落盘的议题;没有则建一个默认议题
    let (mut issues, issue_warns) = crate::issue::load_all_issues(&teamfly_dir)?;
    warns.extend(issue_warns);
    let fresh_start = issues.is_empty();
    if fresh_start {
        issues.push(Issue::new("默认议题"));
        // 这个 id 也得落水位线 —— 关掉它之后分支还会留着
        if let Err(e) = crate::issue::bump_watermark(&teamfly_dir, issues[0].id + 1) {
            warns.push(format!("议题 id 水位线写不进去({e});关议题后重启可能重发已用过的 id"));
        }
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
        pending_delete_note: String::new(),
        show_help: false,
        cancel: tokio_util::sync::CancellationToken::new(),
        team_gen: 0,
        cancel_gen: 0,
    };

    // 多实例检测:写一个 pid 文件,若已有别的活进程在用,警告(不阻止)
    if let Some(warn) = check_instance_lock(&model.teamfly_dir) {
        warns.push(warn);
    }
    // 上次残留的 worktree(崩溃/被杀时来不及清理)
    let stale = crate::worktree::count_stale(&model.work_dir, &model.teamfly_dir);
    if stale > 0 {
        warns.push(format!(
            "有 {stale} 个 agent worktree 留在 .teamfly/worktrees/(对应 teamfly/issue-* 分支)"
        ));
    }
    // 非 git 仓库:worktree 隔离不可用,退回到共用模式
    if !model.work_dir.join(".git").exists() {
        warns.push("工作目录不是 git 仓库,agent 之间不隔离,并发写文件可能冲突".into());
    }
    // agent 在 worktree 里 commit 需要 git 身份;没配的话它一提交就失败,
    // 而改动只留在工作区,用户很难看出发生了什么
    if crate::worktree::missing_git_identity(&model.work_dir) {
        warns.push(
            "git 没配 user.name / user.email,agent 在 worktree 里 commit 会失败".into(),
        );
    }
    // .teamfly/ 下有议题历史和 mcp.json(可能带鉴权 header),必须被 git 忽略 ——
    // 否则 fallback 模式下 agent 一句 `git add -A` 就把它们提交进历史
    use crate::worktree::IgnoreState;
    match crate::worktree::ensure_teamfly_ignored(&model.work_dir) {
        IgnoreState::AlreadyIgnored => {}
        IgnoreState::JustAdded => warns.push(
            ".teamfly/ 未被忽略,已自动加进 .gitignore(里面有议题历史和 MCP 配置)".into(),
        ),
        // 写不进去是最危险的一种:保护没生效,而以前这里和「本来就忽略了」
        // 返回同一个值,用户什么提示都收不到
        IgnoreState::WriteFailed(e) => warns.push(format!(
            "⚠ .teamfly/ 没被 git 忽略,而 .gitignore 写不进去({e})——\
             agent 可能把议题历史和 MCP 配置提交进你的仓库,请手动加一条 .teamfly/"
        )),
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
                // 覆盖成自己的 pid:追加者模式,两边都知道对方在
                let _ = std::fs::write(&lock, format!("{}", std::process::id()));
                return Some(format!(
                    "另一个 teamfly (pid {pid}) 正在用这个目录,两边的议题历史可能互相覆盖,建议只开一个"
                ));
            }
        }
    }

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
