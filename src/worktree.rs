//! Agent worktree 隔离：**一个议题一个** worktree + 分支。
//!
//! 分支：`teamfly/issue-<issue_id>`
//! 目录：`<teamfly_dir>/worktrees/<issue_id>/`
//!
//! 为什么是一议题一个而不是一 agent 一个：
//! - 同议题内的接力是**顺序**的（TPM → DEV → REV），共享一个工作树后
//!   下游直接就能看到上游改的文件，不需要 `git merge <上游分支>` 那一跳
//!   （那一跳既费 token 又依赖 agent 听话）。
//! - 用户采纳时只面对**一个**分支 `teamfly/issue-3`，而不是一堆
//!   `teamfly/DEV/abc` / `teamfly/REV/def` 还得琢磨合哪个、什么顺序。
//! - 磁盘上一个议题只有一份 checkout，不需要数量上限。
//!
//! 代价：同议题内两个写手不能同时跑。由 app.rs 的排队机制处理
//! （第二个写手排队等前一个交卷），跨议题仍然完全并行。
//!
//! 非 git 仓库时整个机制跳过，退回共用 work_dir 的老行为。

use std::path::{Path, PathBuf};
use std::process::Command;

/// worktree 准备结果。
pub struct WorktreeResult {
    /// agent 应该在这个目录里干活。
    /// worktree 建成功了就是 worktree 路径;否则是主 work_dir(fallback)。
    pub agent_dir: PathBuf,
    /// 这一轮的分支名。fallback 时为 None。
    pub branch: Option<String>,
}

/// 议题的分支名。
pub fn issue_branch(issue_id: u64) -> String {
    format!("teamfly/issue-{issue_id}")
}

/// 议题的 worktree 目录。
pub fn issue_dir(teamfly_dir: &Path, issue_id: u64) -> PathBuf {
    teamfly_dir.join("worktrees").join(issue_id.to_string())
}

/// 为某议题准备 worktree（已存在则复用 —— 这正是接力能直接看到上游改动的原因）。
/// 失败不致命：fallback 到主 work_dir。
pub fn prepare(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) -> WorktreeResult {
    let fallback = WorktreeResult {
        agent_dir: work_dir.to_path_buf(),
        branch: None,
    };

    // 非 git 仓库 → fallback
    if !work_dir.join(".git").exists() {
        return fallback;
    }

    let branch = issue_branch(issue_id);
    let wt_dir = issue_dir(teamfly_dir, issue_id);

    // 已存在 → 直接复用。上一轮的改动还在里面，下游接着干就是。
    if wt_dir.exists() {
        return WorktreeResult { agent_dir: wt_dir, branch: Some(branch) };
    }

    // 分支不存在则基于当前 HEAD 建（已存在时这条命令失败，无所谓）
    let _ = Command::new("git")
        .args(["branch", "--no-track", &branch, "HEAD"])
        .current_dir(work_dir)
        .output();

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
        Ok(o) if o.status.success() => WorktreeResult { agent_dir: wt_dir, branch: Some(branch) },
        Ok(o) => {
            eprintln!("git worktree add 失败: {}", String::from_utf8_lossy(&o.stderr).trim());
            fallback
        }
        Err(e) => {
            eprintln!("git worktree add 失败: {e}");
            fallback
        }
    }
}

/// 分支是否存在。
pub fn branch_exists(work_dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(work_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 关议题时的收尾:**不动分支**,只在 worktree 干净时把目录收掉。
///
/// 关议题只是「我不看这个议题了」,不该销毁工作成果 —— 分支留着,
/// 用户随时可以 `git push` 开 MR、`git merge`、或者以后再删。
/// 目录只在没有未提交改动时才删(`worktree remove --force` 会连未提交的一起丢)。
///
/// 返回 (目录是否删了, 是否有未提交改动被保留)。
pub fn release_issue(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) -> (bool, bool) {
    let wt_dir = issue_dir(teamfly_dir, issue_id);
    if !wt_dir.exists() {
        return (false, false);
    }
    // 有未提交的改动 → 目录也留着,不然 --force 会把它们丢掉
    let dirty = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", "HEAD"])
            .current_dir(&wt_dir),
    )
    .is_some()
        || Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&wt_dir)
            .output()
            .ok()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
    if dirty {
        return (false, true);
    }
    let removed = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(work_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    (removed, false)
}

/// 彻底删掉某议题的 worktree **和分支**（`/drop` —— 用户明确表示不要了）。
/// 返回是否真删了。
pub fn remove_issue(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) -> bool {
    let wt_dir = issue_dir(teamfly_dir, issue_id);
    if !wt_dir.exists() {
        return false;
    }
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(work_dir)
        .output();
    let _ = Command::new("git")
        .args(["branch", "-D", &issue_branch(issue_id)])
        .current_dir(work_dir)
        .output();
    true
}

/// 统计残留的 worktree 数量（启动时提示用户）。
pub fn count_stale(teamfly_dir: &Path) -> usize {
    std::fs::read_dir(teamfly_dir.join("worktrees"))
        .map(|d| d.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0)
}

/// worktree 里一点改动都没有?(既没 commit、也没改工作区、也没新增文件)
fn is_untouched(wt_dir: &Path, work_dir: &Path, branch: &str) -> bool {
    // 有 commit?
    if last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
            .current_dir(work_dir),
    )
    .is_some()
    {
        return false;
    }
    // 工作区有改动?(含 staged)
    if last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", "HEAD"])
            .current_dir(wt_dir),
    )
    .is_some()
    {
        return false;
    }
    // 有新增未跟踪文件?
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(wt_dir)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    !untracked
}

/// 交卷后回收:整个议题至今一个字都没改过就删掉 worktree 和分支。返回是否删了。
///
/// 纯查询类议题(「看一下 X」)不该留下一个完整 checkout 占磁盘。
pub fn drop_if_untouched(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) -> bool {
    let wt_dir = issue_dir(teamfly_dir, issue_id);
    let branch = issue_branch(issue_id);
    if !wt_dir.exists() || !is_untouched(&wt_dir, work_dir, &branch) {
        return false;
    }
    remove_issue(work_dir, teamfly_dir, issue_id)
}

/// worktree 里改了什么。
///
/// 必须同时看**已提交**和**未提交**的改动:agent 经常只改文件不 commit,
/// 只看 `git diff HEAD...<branch>`(比 commit)会显示「无改动」,
/// 用户以为 agent 啥也没干,其实文件已经改了。
pub fn change_summary(wt_dir: &Path, work_dir: &Path, branch: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(stat) = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
            .current_dir(work_dir),
    ) {
        parts.push(format!("已提交 {stat}"));
    }
    if let Some(stat) = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", "HEAD"])
            .current_dir(wt_dir),
    ) {
        parts.push(format!("未提交 {stat}"));
    }
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(wt_dir)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);
    if untracked > 0 {
        parts.push(format!("新增 {untracked} 个文件"));
    }

    if parts.is_empty() {
        "无改动".to_string()
    } else {
        parts.join(" · ")
    }
}

fn last_stat_line(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().last()?.trim();
    if line.is_empty() { None } else { Some(line.to_string()) }
}

/// 检查 git 提交身份是否配好。没配的话 agent 一 commit 就失败,
/// 而改动只留在工作区、汇报里也看不出来,用户完全不知道发生了什么。
pub fn missing_git_identity(work_dir: &Path) -> bool {
    for key in ["user.name", "user.email"] {
        let ok = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(work_dir)
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false);
        if !ok {
            return true;
        }
    }
    false
}

/// 确保 `.teamfly/` 被 git 忽略。返回是否新写入了规则。
///
/// `.teamfly/env.toml` 里放的是 API key。用户项目没 ignore 它的话,
/// 它会以未跟踪文件出现在 `git status` 里,agent 一句 `git add -A`
/// 就把密钥提交进历史了(fallback 模式下 agent 就在主目录干活)。
pub fn ensure_teamfly_ignored(work_dir: &Path) -> bool {
    if !work_dir.join(".git").exists() {
        return false;
    }
    let ignored = Command::new("git")
        .args(["check-ignore", "-q", ".teamfly/"])
        .current_dir(work_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ignored {
        return false;
    }
    // 已有 .teamfly/ 规则(哪怕被否定规则覆盖)就不动 ——
    // 用户自己配了 !.teamfly/ 说明他有意要追踪它,不该替他改。
    let path = work_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".teamfly/") {
        return false;
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# teamfly 的本地状态(含 API key,不要提交)\n.teamfly/\n");
    std::fs::write(&path, content).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关议题**不能**删分支 —— 关掉只是「不看了」,不该销毁工作成果。
    /// 用户可能还想 git push 开 MR。
    #[test]
    fn release_issue_keeps_branch() {
        let root = std::env::temp_dir().join(format!("tf_rel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&root).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 5);
        assert!(wt.branch.is_some(), "该建出 worktree");
        // agent 干活并提交
        std::fs::write(wt.agent_dir.join("a.txt"), "v2").unwrap();
        Command::new("git").args(["commit", "-qam", "agent 干的活"])
            .current_dir(&wt.agent_dir).output().unwrap();

        let branch = issue_branch(5);
        let (removed, dirty) = release_issue(&root, &tf, 5);
        assert!(removed, "干净的 worktree 目录该被收掉");
        assert!(!dirty);
        // 分支必须还在,agent 干的活才没丢
        assert!(branch_exists(&root, &branch), "关议题不该删分支!");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 有未提交改动时,worktree 目录也得留着 ——
    /// `worktree remove --force` 会把未提交的改动一起丢掉。
    #[test]
    fn release_issue_keeps_dirty_worktree() {
        let root = std::env::temp_dir().join(format!("tf_dirty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&root).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 6);
        // 改了但**没提交**
        std::fs::write(wt.agent_dir.join("a.txt"), "未提交的改动").unwrap();

        let (removed, dirty) = release_issue(&root, &tf, 6);
        assert!(!removed, "有未提交改动时不该删目录");
        assert!(dirty, "该报告存在未提交改动");
        assert!(wt.agent_dir.exists(), "目录必须还在");
        assert_eq!(
            std::fs::read_to_string(wt.agent_dir.join("a.txt")).unwrap(),
            "未提交的改动"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn branch_and_dir_are_per_issue() {
        assert_eq!(issue_branch(3), "teamfly/issue-3");
        assert_eq!(
            issue_dir(Path::new("/p/.teamfly"), 3),
            PathBuf::from("/p/.teamfly/worktrees/3")
        );
        // 不同议题必须分开
        assert_ne!(issue_branch(3), issue_branch(4));
        assert_ne!(
            issue_dir(Path::new("/p"), 3),
            issue_dir(Path::new("/p"), 4)
        );
    }
}
