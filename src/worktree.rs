//! Agent worktree 隔离：每个 agent 每个议题一个独立 worktree + 分支。
//!
//! 分支命名：`teamfly/<agent_name>/<short_commit_hash>`
//! worktree 路径：`<teamfly_dir>/worktrees/<agent_name>-<short_hash>/`
//!
//! 非 git 仓库时整个机制跳过，退回共用 work_dir 的老行为。

use std::path::{Path, PathBuf};
use std::process::Command;

/// worktree 准备结果。
pub struct WorktreeResult {
    /// agent 应该在这个目录里干活。
    /// 如果 worktree 建成功了就是 worktree 路径;否则是主 work_dir(fallback)。
    pub agent_dir: PathBuf,
}

/// 为 agent 准备 worktree。如果已存在则复用(在上一轮的基础上继续)。
///
/// 失败不致命：返回 fallback 到主 work_dir。
pub fn prepare(
    work_dir: &Path,
    teamfly_dir: &Path,
    agent_name: &str,
) -> WorktreeResult {
    let fallback = WorktreeResult {
        agent_dir: work_dir.to_path_buf(),
    };

    // 非 git 仓库 → fallback
    if !work_dir.join(".git").exists() {
        return fallback;
    }

    let short_hash = get_short_hash(work_dir);
    let branch = format!("teamfly/{agent_name}/{short_hash}");
    let wt_dir = worktree_path_with_hash(teamfly_dir, agent_name, &short_hash);

    // 同名 worktree 已存在(极少见:同一 agent 在同一 commit 上连续被派两次活)→ 复用
    if wt_dir.exists() {
        return WorktreeResult { agent_dir: wt_dir };
    }

    // 确保分支存在(不存在则基于 HEAD 创建)
    let _ = Command::new("git")
        .args(["branch", "--no-track", &branch, "HEAD"])
        .current_dir(work_dir)
        .output();

    // 建 worktree
    if let Some(parent) = wt_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = Command::new("git")
        .args(["worktree", "add", "--force"])
        .arg(&wt_dir)
        .arg(&branch)
        .current_dir(work_dir)
        .output();

    match out {
        Ok(o) if o.status.success() => WorktreeResult { agent_dir: wt_dir },
        Ok(o) => {
            eprintln!(
                "git worktree add 失败: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            fallback
        }
        Err(e) => {
            eprintln!("git worktree add 失败: {e}");
            fallback
        }
    }
}

/// worktree 目录路径(带 hash 版,用于新建)。
pub fn worktree_path_with_hash(teamfly_dir: &Path, agent_name: &str, hash: &str) -> PathBuf {
    teamfly_dir
        .join("worktrees")
        .join(format!("{agent_name}-{hash}"))
}

/// 删除指定的 worktree 目录 + 对应分支。
fn remove_dir(work_dir: &Path, wt_dir: &Path) {
    if wt_dir.exists() {
        let branch = read_worktree_branch(wt_dir);
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(wt_dir)
            .current_dir(work_dir)
            .output();
        if let Some(b) = branch {
            let _ = Command::new("git")
                .args(["branch", "-D", &b])
                .current_dir(work_dir)
                .output();
        }
    }
}

/// 删除某个 agent 的所有 worktree(按名字前缀匹配)。
pub fn remove_agent(work_dir: &Path, teamfly_dir: &Path, agent_name: &str) {
    let dir = teamfly_dir.join("worktrees");
    let prefix = format!("{agent_name}-");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&prefix) && e.path().is_dir() {
                    remove_dir(work_dir, &e.path());
                }
            }
        }
    }
}

/// 删除所有 worktree(关闭议题时)。
pub fn remove_all(work_dir: &Path, teamfly_dir: &Path) {
    let dir = teamfly_dir.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                remove_dir(work_dir, &e.path());
            }
        }
    }
}

/// 读取 worktree 里当前所在的分支名。
fn read_worktree_branch(wt_dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(wt_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() || b == "HEAD" { None } else { Some(b) }
}

/// 获取 HEAD 的短 hash(用于分支命名)。
fn get_short_hash(work_dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(work_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// 列出残留的 worktree 目录(启动时提示用户)。
pub fn list_stale(teamfly_dir: &Path) -> Vec<String> {
    let dir = teamfly_dir.join("worktrees");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// 找到某个 agent 最新的 worktree 目录和分支名(用于汇报时显示)。
pub fn latest_for(teamfly_dir: &Path, agent_name: &str) -> Option<(PathBuf, String)> {
    let dir = teamfly_dir.join("worktrees");
    let prefix = format!("{agent_name}-");
    let mut latest: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&prefix) && e.path().is_dir() {
                    // 取最新的(按名字排序,hash 不保证时间序,用修改时间)
                    let p = e.path();
                    if latest.as_ref().is_none_or(|prev| {
                        p.metadata().and_then(|m| m.modified()).ok()
                            > prev.metadata().and_then(|m| m.modified()).ok()
                    }) {
                        latest = Some(p);
                    }
                }
            }
        }
    }
    let wt_dir = latest?;
    let branch = read_worktree_branch(&wt_dir)?;
    Some((wt_dir, branch))
}

/// 获取 worktree 里相对于基准的 diff 摘要(几个文件改了)。
pub fn diff_summary(work_dir: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .args(["diff", "--stat", &format!("HEAD...{branch}")])
        .current_dir(work_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let last = s.lines().last().unwrap_or("").trim();
            if last.is_empty() { "无文件改动".to_string() } else { last.to_string() }
        }
        _ => "无文件改动".to_string(),
    }
}
