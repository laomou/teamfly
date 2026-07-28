//! CLI:teamfly work [工作目录] [--team <团队文件夹>]

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
        /// 团队文件夹
        #[arg(long)]
        team: Option<PathBuf>,
    },
}

/// 组装初始 Model。
pub fn build(dir: Option<PathBuf>, team_arg: Option<PathBuf>) -> Result<(Model, Vec<String>)> {
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
    let warns = team::preflight(&team);

    // 恢复落盘的议题;没有则建一个默认议题
    let mut issues = crate::issue::load_all_issues(&teamfly_dir)?;
    if issues.is_empty() {
        issues.push(Issue::new("默认议题"));
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
        pending_delete: None,
    };

    Ok((model, warns))
}

fn resolve_team_dir(team_arg: Option<PathBuf>, teamfly_dir: &std::path::Path) -> Result<PathBuf> {
    let teams_dir = teamfly_dir.join("teams");

    if let Some(t) = team_arg {
        // 优先当「名字」解析:.teamfly/teams/<名>
        let by_name = teams_dir.join(&t);
        if by_name.is_dir() {
            return Ok(by_name);
        }
        // 兼容:也允许直接给一个存在的路径
        if t.is_dir() {
            return Ok(t);
        }
        bail!(
            "找不到团队「{}」。它应是 {} 下的一个子目录名。\n现有团队:{}",
            t.display(),
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
