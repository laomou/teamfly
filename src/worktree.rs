//! Agent worktree 隔离：每个 agent 每轮活一个独立 worktree + 分支。
//!
//! 分支命名：`teamfly/<agent_name>/<short_commit_hash>`
//! worktree 目录：`<teamfly_dir>/worktrees/<issue_id>/<agent_name>-<short_hash>/`
//!
//! 目录里带 `issue_id` 这一层是必须的：关闭议题时要只删属于它的 worktree。
//! 以前扁平放在 `worktrees/<name>-<hash>/`，关一个议题就得删全部，
//! 把别的议题里用户还没采纳的改动一起干掉了。
//!
//! 非 git 仓库时整个机制跳过，退回共用 work_dir 的老行为。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 单个议题最多留多少个 worktree。每轮派活都新建一个完整 checkout,
/// 不设顶的话跑一天几十轮就把磁盘吃满。超出后删最旧的。
pub const WORKTREE_CAP: usize = 8;

/// worktree 准备结果。
pub struct WorktreeResult {
    /// agent 应该在这个目录里干活。
    /// worktree 建成功了就是 worktree 路径;否则是主 work_dir(fallback)。
    pub agent_dir: PathBuf,
    /// 这一轮的分支名。fallback 时为 None。
    pub branch: Option<String>,
}

/// 为 agent 准备这一轮的 worktree。失败不致命：fallback 到主 work_dir。
pub fn prepare(work_dir: &Path, teamfly_dir: &Path, issue_id: u64, agent_name: &str) -> WorktreeResult {
    let fallback = WorktreeResult {
        agent_dir: work_dir.to_path_buf(),
        branch: None,
    };

    // 非 git 仓库 → fallback
    if !work_dir.join(".git").exists() {
        return fallback;
    }

    let short_hash = get_short_hash(work_dir);
    let wt_dir = issue_dir(teamfly_dir, issue_id).join(format!("{agent_name}-{short_hash}"));

    // 同一 agent 在同一 commit 上又被派了一轮(同议题)→ 复用,改动接着攒
    if wt_dir.exists() {
        if let Some(b) = read_worktree_branch(&wt_dir) {
            return WorktreeResult { agent_dir: wt_dir, branch: Some(b) };
        }
    }

    // 挑一个没被占用的分支名。同一 agent 在**不同议题**里从同一 commit 分叉时
    // 会撞名 —— 不处理的话 worktree add 会失败或 checkout 到别议题那个分支上,
    // 两个议题的改动混进同一个分支。撞了就加 -2 / -3 后缀。
    let branch = pick_free_branch(work_dir, agent_name, &short_hash);

    // 先腾地方,再建新的
    enforce_cap(work_dir, teamfly_dir, issue_id);

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

/// 某个议题的 worktree 根目录。
fn issue_dir(teamfly_dir: &Path, issue_id: u64) -> PathBuf {
    teamfly_dir.join("worktrees").join(issue_id.to_string())
}

/// 挑一个还没被占用的分支名:`teamfly/<agent>/<hash>`,撞了就 `-2`、`-3`…
fn pick_free_branch(work_dir: &Path, agent_name: &str, short_hash: &str) -> String {
    let base = format!("teamfly/{agent_name}/{short_hash}");
    if !branch_exists(work_dir, &base) {
        return base;
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !branch_exists(work_dir, &cand) {
            return cand;
        }
    }
    base // 理论上到不了
}

fn branch_exists(work_dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(work_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 列出某议题下所有 worktree 目录，按修改时间从旧到新。
fn list_in_issue(teamfly_dir: &Path, issue_id: u64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(issue_dir(teamfly_dir, issue_id)) else {
        return vec![];
    };
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let t = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((t, e.path()))
        })
        .collect();
    v.sort_by_key(|(t, _)| *t);
    v.into_iter().map(|(_, p)| p).collect()
}

/// 超出上限时删最旧的，给新的腾地方。
fn enforce_cap(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) {
    let existing = list_in_issue(teamfly_dir, issue_id);
    // 留 CAP-1 个位置给即将新建的那个
    let keep = WORKTREE_CAP.saturating_sub(1);
    if existing.len() <= keep {
        return;
    }
    for old in &existing[..existing.len() - keep] {
        remove_dir(work_dir, old);
    }
}

/// 删除指定的 worktree 目录 + 它所在的分支。
fn remove_dir(work_dir: &Path, wt_dir: &Path) {
    if !wt_dir.exists() {
        return;
    }
    // 分支名要在删目录之前读出来（删完就读不到了）
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

/// 删除某议题下某个 agent 的所有 worktree。
///
/// 目录名按 `<agent>-<hash>` 精确切分来比对，不用前缀匹配 ——
/// 前缀 `"DEV-"` 会把 `DEV-BE-abc123` 也吃掉，误删另一个成员的改动。
pub fn remove_agent(work_dir: &Path, teamfly_dir: &Path, issue_id: u64, agent_name: &str) -> usize {
    let mut n = 0;
    for p in list_in_issue(teamfly_dir, issue_id) {
        if dir_agent_name(&p).as_deref() == Some(agent_name) {
            remove_dir(work_dir, &p);
            n += 1;
        }
    }
    n
}

/// 从 worktree 目录名里取出 agent 名（去掉末尾的 `-<hash>`）。
/// agent 名本身可以含 `-`，所以从**最后**一个 `-` 切。
fn dir_agent_name(p: &Path) -> Option<String> {
    let name = p.file_name()?.to_str()?;
    let (agent, _hash) = name.rsplit_once('-')?;
    Some(agent.to_string())
}

/// 删除某个议题的所有 worktree（关闭议题时）。只碰这个议题的，别的议题不动。
pub fn remove_issue(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) {
    for p in list_in_issue(teamfly_dir, issue_id) {
        remove_dir(work_dir, &p);
    }
    let _ = std::fs::remove_dir(issue_dir(teamfly_dir, issue_id));
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

/// 获取 HEAD 的短 hash（用于分支命名）。
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

/// 统计所有议题下残留的 worktree 数量（启动时提示用户）。
pub fn count_stale(teamfly_dir: &Path) -> usize {
    let Ok(issues) = std::fs::read_dir(teamfly_dir.join("worktrees")) else {
        return 0;
    };
    issues
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            std::fs::read_dir(e.path())
                .map(|d| d.flatten().filter(|x| x.path().is_dir()).count())
                .unwrap_or(0)
        })
        .sum()
}

/// worktree 里一点改动都没有?(既没 commit、也没改工作区、也没新增文件)
///
/// 纯查询类任务(「看一下 X」「解释一下 Y」)很常见,它们不该留下一个
/// 完整 checkout 占着磁盘,交卷时直接删掉。
fn is_untouched(wt_dir: &Path, work_dir: &Path, branch: &str) -> bool {
    // 有 commit?
    let committed = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
            .current_dir(work_dir),
    );
    if committed.is_some() {
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

/// 交卷后回收:worktree 里没有任何改动就删掉它和分支。返回是否删了。
pub fn drop_if_untouched(work_dir: &Path, wt_dir: &Path, branch: &str) -> bool {
    if !wt_dir.exists() || !is_untouched(wt_dir, work_dir, branch) {
        return false;
    }
    remove_dir(work_dir, wt_dir);
    true
}

/// worktree 里改了什么。
///
/// 必须同时看**已提交**和**未提交**的改动:agent 经常只改文件不 commit,
/// 只看 `git diff HEAD...<branch>`(比 commit)会显示「无文件改动」,
/// 用户以为 agent 啥也没干,其实文件已经改了。
pub fn change_summary(wt_dir: &Path, work_dir: &Path, branch: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // 已提交的:主仓库里比 HEAD 和分支
    if let Some(stat) = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
            .current_dir(work_dir),
    ) {
        parts.push(format!("已提交 {stat}"));
    }
    // 未提交的:worktree 里比它自己的 HEAD(含 staged 与 unstaged)
    if let Some(stat) = last_stat_line(
        Command::new("git")
            .args(["diff", "--shortstat", "HEAD"])
            .current_dir(wt_dir),
    ) {
        parts.push(format!("未提交 {stat}"));
    }
    // 新增的未跟踪文件
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
    // git 已经认为它被忽略了 → 不用管
    let ignored = Command::new("git")
        .args(["check-ignore", "-q", ".teamfly/"])
        .current_dir(work_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ignored {
        return false;
    }
    // 追加到项目 .gitignore
    let path = work_dir.join(".gitignore");
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# teamfly 的本地状态(含 API key,不要提交)\n.teamfly/\n");
    std::fs::write(&path, content).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_agent_name_splits_at_last_dash() {
        // agent 名本身含 - 时也要切对,否则 /drop 会误删
        assert_eq!(dir_agent_name(Path::new("/x/DEV-abc123")).as_deref(), Some("DEV"));
        assert_eq!(dir_agent_name(Path::new("/x/DEV-BE-abc123")).as_deref(), Some("DEV-BE"));
        assert_eq!(dir_agent_name(Path::new("/x/小盾-abc123")).as_deref(), Some("小盾"));
        assert_eq!(dir_agent_name(Path::new("/x/nodash")), None);
    }
}


