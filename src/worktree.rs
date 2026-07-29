//! Agent worktree 隔离：每个 agent 每个议题一个独立 worktree + 分支。
//!
//! 分支命名：`teamfly/<issue_id>/<agent_name>`
//! worktree 路径：`<teamfly_dir>/worktrees/<issue_id>-<agent_name>/`
//!
//! 非 git 仓库时整个机制跳过，退回共用 work_dir 的老行为。

use std::path::{Path, PathBuf};
use std::process::Command;

/// worktree 准备结果。
pub struct WorktreeResult {
    /// agent 应该在这个目录里干活。
    /// 如果 worktree 建成功了就是 worktree 路径;否则是主 work_dir(fallback)。
    pub agent_dir: PathBuf,
    /// 分支名(如果建了的话)
    pub branch: Option<String>,
}

/// 为 agent 准备 worktree。如果已存在则复用(在上一轮的基础上继续)。
///
/// 失败不致命：返回 fallback 到主 work_dir。
pub fn prepare(
    work_dir: &Path,
    teamfly_dir: &Path,
    issue_id: u64,
    agent_name: &str,
) -> WorktreeResult {
    let fallback = WorktreeResult {
        agent_dir: work_dir.to_path_buf(),
        branch: None,
    };

    // 非 git 仓库 → fallback
    if !work_dir.join(".git").exists() {
        return fallback;
    }

    let branch = format!("teamfly/{issue_id}/{agent_name}");
    let wt_dir = worktree_path(teamfly_dir, issue_id, agent_name);

    // worktree 已存在 → 复用(上次的改动还在里面，agent 可以在上面继续)
    if wt_dir.exists() {
        return WorktreeResult {
            agent_dir: wt_dir,
            branch: Some(branch),
        };
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
        Ok(o) if o.status.success() => WorktreeResult {
            agent_dir: wt_dir,
            branch: Some(branch),
        },
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

/// worktree 目录路径。
pub fn worktree_path(teamfly_dir: &Path, issue_id: u64, agent_name: &str) -> PathBuf {
    teamfly_dir
        .join("worktrees")
        .join(format!("{issue_id}-{agent_name}"))
}

/// 删除某个 agent 在某议题的 worktree + 分支。
pub fn remove(work_dir: &Path, teamfly_dir: &Path, issue_id: u64, agent_name: &str) {
    let wt_dir = worktree_path(teamfly_dir, issue_id, agent_name);
    if wt_dir.exists() {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_dir)
            .current_dir(work_dir)
            .output();
    }
    let branch = format!("teamfly/{issue_id}/{agent_name}");
    let _ = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(work_dir)
        .output();
}

/// 删除某个议题的所有 worktree + 分支(关闭议题时调用)。
pub fn remove_issue(work_dir: &Path, teamfly_dir: &Path, issue_id: u64, members: &[String]) {
    for name in members {
        remove(work_dir, teamfly_dir, issue_id, name);
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
