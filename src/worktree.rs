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
fn issue_dir(teamfly_dir: &Path, issue_id: u64) -> PathBuf {
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

    // 已存在 → 复用,但**必须先确认它真是个有效 worktree**。
    //
    // 只看 `wt_dir.exists()` 是危险的:这个目录在主仓库**里面**,
    // 一旦 `.git` 文件丢了(上次 remove 半途失败 / 用户手删),
    // git 的向上发现会直接命中**主仓库** —— agent 以为自己在隔离环境里,
    // 实际 `git add -A && git commit` 打到用户当前分支上,
    // 把用户没提交的改动一起打包进去(实测过)。
    if wt_dir.exists() {
        if is_valid_worktree(&wt_dir, work_dir) {
            return WorktreeResult { agent_dir: wt_dir, branch: Some(branch) };
        }
        // 残壳:先 prune 掉注册信息再重建。修不好就 fallback,
        // 绝不能把这个目录交给 agent。
        let _ = Command::new("git").args(["worktree", "prune"]).current_dir(work_dir).output();
        if std::fs::remove_dir_all(&wt_dir).is_err() {
            eprintln!(
                "{} 不是有效的 git worktree 且清不掉,这一轮退回主工作目录",
                wt_dir.display()
            );
            return fallback;
        }
    }

    // 分支不存在则基于当前 HEAD 建。
    // 已存在时这条会失败 —— 那可能是**上一个用过同一 id 的议题**留下的分支
    // (关议题故意保留分支,而议题 id 会随 jsonl 删除被回收)。
    // 直接 checkout 上去,新议题的 agent 就站在别人的成果上干活了。
    if branch_exists(work_dir, &branch) {
        eprintln!(
            "分支 {branch} 已存在(可能是用过同一议题 id 的旧议题留下的),\
             为避免两个议题的改动混在一起,这一轮退回主工作目录"
        );
        return fallback;
    }
    let created = Command::new("git")
        .args(["branch", "--no-track", &branch, "HEAD"])
        .current_dir(work_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !created {
        eprintln!("建分支 {branch} 失败,这一轮退回主工作目录");
        return fallback;
    }

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

/// 这个目录真的是**属于本仓库**的 git worktree 吗?
///
/// 光有 `.git` 不够:得确认它指回同一个仓库。否则 agent 会在一个
/// 「看起来像 worktree、实际直通主仓库」的目录里干活。
fn is_valid_worktree(wt_dir: &Path, work_dir: &Path) -> bool {
    // worktree 的 .git 是**文件**(内容 `gitdir: ...`),不是目录
    if !wt_dir.join(".git").is_file() {
        return false;
    }
    let common = |d: &Path| {
        stdout_of(
            Command::new("git")
                .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .current_dir(d),
        )
        .and_then(|s| std::fs::canonicalize(s.trim()).ok())
    };
    match (common(wt_dir), common(work_dir)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
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

/// 彻底删掉某议题的 worktree **和分支**（改动确认无价值时才用）。
/// 返回是否真删了。
fn remove_issue(work_dir: &Path, teamfly_dir: &Path, issue_id: u64) -> bool {
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
///
/// **只有在能确定「真的什么都没有」时才返回 true** —— 调用方拿它去删分支,
/// 判错一次就是永久丢数据。任何一个 git 命令跑不通(HEAD 解析不了、
/// 和议题分支求不出合并基线、仓库状态异常)都返回 false:不确定就不删。
fn is_untouched(wt_dir: &Path, work_dir: &Path, branch: &str) -> bool {
    // 有 commit?用 rev-list 数提交个数,不用 --shortstat 量内容差 ——
    // 「加了文件又删掉」这种净差为空但有真实提交历史(commit message 里
    // 可能就是 agent 的结论)的情况,--shortstat 会输出空而被误判成没改动。
    match stdout_of(
        Command::new("git")
            .args(["rev-list", "--count", branch, "--not", "HEAD"])
            .current_dir(work_dir),
    ) {
        // 数得出来且为 0 才算没提交
        Some(n) => {
            if n.trim() != "0" {
                return false;
            }
        }
        // 数不出来(HEAD 无效 / 无合并基线 / 分支不存在)→ 不确定,不删
        None => return false,
    }
    // 工作区有改动?(含 staged;porcelain 空 = 干净)
    // 顺带覆盖 .gitignore 命中的产物之外的一切;--porcelain 失败同样视为「不确定」。
    match stdout_of(
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(wt_dir),
    ) {
        Some(s) => {
            if !s.trim().is_empty() {
                return false;
            }
        }
        None => return false,
    }
    // HEAD 是不是 detached / 正在 rebase-merge?
    // detached 上的提交不挂在任何分支上,rev-list 数不到它 ——
    // 删掉 worktree 管理目录那些提交就彻底不可达了。
    if wt_dir.join(".git").exists() {
        let git_dir = stdout_of(
            Command::new("git")
                .args(["rev-parse", "--git-dir"])
                .current_dir(wt_dir),
        );
        if let Some(gd) = git_dir {
            let gd = Path::new(gd.trim());
            // rebase / merge / cherry-pick 中途:状态目录还在,不能删
            for marker in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
                if gd.join(marker).exists() {
                    return false;
                }
            }
        }
        // detached HEAD?(symbolic-ref 在 detached 时失败)
        let attached = Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(wt_dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !attached {
            return false;
        }
    }
    true
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

/// 取命令 stdout。**命令失败返回 None,和「成功但输出为空」区分开** ——
/// 以前两者都是 None,调用方把「git 跑不通」当成「没有改动」,
/// 进而把 agent 已提交的成果连分支一起删掉。
fn stdout_of(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `--shortstat` 的最后一行。空输出(没改动)和命令失败都返回 None ——
/// 这里只用于**展示**,两者都显示成「没这一项」是可以接受的。
/// 判断「能不能删」必须用 `stdout_of`,别用这个。
fn last_stat_line(cmd: &mut Command) -> Option<String> {
    let s = stdout_of(cmd)?;
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
/// `.teamfly/` 下有议题历史和 `mcp.json`(可能带鉴权 header)。没 ignore 的话,
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

    /// 建一个带一次提交的仓库,返回路径。
    fn repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tf_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for a in [
            vec!["init", "-q", "."],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git").args(&a).current_dir(&root).output().unwrap();
        }
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&root).output().unwrap();
        Command::new("git").args(["commit", "-qm", "init"]).current_dir(&root).output().unwrap();
        root
    }

    fn git_in(d: &Path, a: &[&str]) -> String {
        let o = Command::new("git").args(a).current_dir(d).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// git 命令跑不通时**绝不能**判成「没改动」—— 那会 `git branch -D`
    /// 把 agent 已提交的成果删掉,且界面上一个字都不提。
    ///
    /// 触发:用户在主工作目录切到无关历史(orphan 分支 / amend 根提交),
    /// `git rev-list <branch> --not HEAD` 求不出结果。
    /// 而此时 worktree 恰恰是**干净**的 —— 因为 agent 老老实实提交了。
    #[test]
    fn untouched_is_false_when_git_cannot_answer() {
        let root = repo("d1");
        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 5);
        assert!(wt.branch.is_some(), "该建出 worktree");

        // agent 按 HANDOFF_NOTE 的指示提交
        std::fs::write(wt.agent_dir.join("feature.py"), "agent 的成果").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&wt.agent_dir).output().unwrap();
        Command::new("git").args(["commit", "-qm", "agent 干的活"])
            .current_dir(&wt.agent_dir).output().unwrap();
        let sha = git_in(&wt.agent_dir, &["rev-parse", "HEAD"]);

        // 用户在主目录切到无关历史 → HEAD 和议题分支求不出关系
        Command::new("git").args(["checkout", "-q", "--orphan", "gh-pages"])
            .current_dir(&root).output().unwrap();
        Command::new("git").args(["rm", "-rq", "--cached", "."])
            .current_dir(&root).output().unwrap();

        assert!(
            !drop_if_untouched(&root, &tf, 5),
            "git 答不上来时必须保守:不能删"
        );
        assert!(branch_exists(&root, &issue_branch(5)), "分支被删了,agent 的成果丢了!");
        assert!(
            !git_in(&root, &["branch", "--contains", &sha]).is_empty(),
            "提交不再挂在任何分支上"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 有真实提交但「净内容差为空」(加了文件又删掉)也不能删 ——
    /// commit message 里可能就是 agent 的结论。
    /// 以前用 `--shortstat` 量内容差,这种情况输出空 → 误判无改动。
    #[test]
    fn untouched_is_false_when_commits_net_to_nothing() {
        let root = repo("d1b");
        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 6);
        let g = |a: &[&str]| { Command::new("git").args(a).current_dir(&wt.agent_dir).output().unwrap(); };
        std::fs::write(wt.agent_dir.join("tmp.py"), "x").unwrap();
        g(&["add", "."]); g(&["commit", "-qm", "加个临时文件"]);
        std::fs::remove_file(wt.agent_dir.join("tmp.py")).unwrap();
        g(&["add", "-A"]); g(&["commit", "-qm", "结论:这条路走不通,已回滚"]);

        assert!(!drop_if_untouched(&root, &tf, 6), "有提交历史就不能删");
        assert!(branch_exists(&root, &issue_branch(6)), "结论被删掉了");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// detached HEAD 上的提交不挂在任何分支上,删掉 worktree 就彻底不可达。
    #[test]
    fn untouched_is_false_on_detached_head_commit() {
        let root = repo("d1c");
        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 7);
        let g = |a: &[&str]| { Command::new("git").args(a).current_dir(&wt.agent_dir).output().unwrap(); };
        g(&["checkout", "-q", "--detach"]);
        std::fs::write(wt.agent_dir.join("big.py"), "重要成果").unwrap();
        g(&["add", "."]); g(&["commit", "-qm", "detached 上的活"]);

        assert!(!drop_if_untouched(&root, &tf, 7), "detached 上有提交,不能删");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 残壳目录(.git 丢了)绝不能交给 agent —— 它在主仓库**里面**,
    /// git 向上发现会命中主仓库,agent 的 `git add -A && git commit`
    /// 会把用户未提交的改动提交到用户当前分支上。
    #[test]
    fn husk_worktree_is_not_reused() {
        let root = repo("d4");
        let tf = root.join(".teamfly");
        let wt = prepare(&root, &tf, 8);
        let dir = wt.agent_dir.clone();
        assert_ne!(dir, root, "第一次该建出真 worktree");

        // 制造残壳:删掉 .git 文件,目录留着
        std::fs::remove_file(dir.join(".git")).unwrap();

        // 用户此刻在主目录有未提交的改动
        std::fs::write(root.join("a.txt"), "用户正在写的代码").unwrap();
        std::fs::write(root.join("user_wip.py"), "用户的新文件").unwrap();

        let again = prepare(&root, &tf, 8);
        // 要么重建成有效 worktree,要么退回主目录并明示 —— 都不能是「拿着残壳还谎报 branch」
        if again.branch.is_some() {
            assert!(
                is_valid_worktree(&again.agent_dir, &root),
                "报告了 branch 就必须是有效 worktree"
            );
        } else {
            assert_eq!(again.agent_dir, root, "fallback 就该老实指向主目录");
        }

        // 无论走哪条,用户的改动都不能被 agent 提交掉
        let before = git_in(&root, &["log", "-1", "--format=%s"]);
        Command::new("git").args(["add", "-A"]).current_dir(&again.agent_dir).output().unwrap();
        Command::new("git").args(["commit", "-qm", "agent 的活"])
            .current_dir(&again.agent_dir).output().unwrap();
        if again.branch.is_some() {
            assert_eq!(
                git_in(&root, &["log", "-1", "--format=%s"]), before,
                "agent 的提交落到了用户的分支上!"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 分支已存在(上一个用过同一 id 的议题留下的)时不能直接 checkout 上去 ——
    /// 否则新议题的 agent 站在别人的成果上干活,两个议题的改动混在一起。
    #[test]
    fn existing_branch_is_not_silently_reused() {
        let root = repo("d3");
        let tf = root.join(".teamfly");
        // 模拟:上个议题 9 干过活并留下分支,worktree 目录已收掉
        let wt = prepare(&root, &tf, 9);
        std::fs::write(wt.agent_dir.join("上个议题的成果.py"), "x").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&wt.agent_dir).output().unwrap();
        Command::new("git").args(["commit", "-qm", "上个议题"])
            .current_dir(&wt.agent_dir).output().unwrap();
        let (removed, _) = release_issue(&root, &tf, 9);
        assert!(removed, "干净的目录该被收掉");
        assert!(branch_exists(&root, &issue_branch(9)), "分支按设计留着");

        // 新议题拿到回收的 id 9
        let fresh = prepare(&root, &tf, 9);
        if fresh.branch.is_some() {
            assert!(
                !fresh.agent_dir.join("上个议题的成果.py").exists(),
                "新议题继承了上个议题的成果"
            );
        } else {
            assert_eq!(fresh.agent_dir, root, "拒绝时该 fallback 到主目录");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

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
