//! issue 落盘与增量前情:jsonl 追加/重放,拼装被唤醒 agent 的群聊前情。

use crate::model::{ChatMsg, Issue, Member};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// issues 目录:<teamfly_dir>/issues
fn issues_dir(teamfly_dir: &Path) -> PathBuf {
    teamfly_dir.join("issues")
}

/// 落盘文件名:`<id>-<名字>.jsonl`。
///
/// 名字里带 id 是为了让 id **跨重启稳定** —— worktree 目录和分支都按 id 命名,
/// 重启后重排的话议题就会去找不属于它的 worktree。顺带也解决了改名要 rename
/// 文件、以及只差大小写的两个议题撞同一个文件的问题(id 不同,文件就不同)。
fn issue_path(teamfly_dir: &Path, id: u64, name: &str) -> PathBuf {
    issues_dir(teamfly_dir).join(format!("{id}-{name}.jsonl"))
}

/// 从文件名解析 `(id, 名字)`。旧格式(无 `<id>-` 前缀)返回 None。
fn parse_stem(stem: &str) -> Option<(u64, String)> {
    let (id_part, name) = stem.split_once('-')?;
    let id: u64 = id_part.parse().ok()?;
    Some((id, name.to_string()))
}

/// 追加一条群聊消息到落盘文件。
pub fn append_chat(teamfly_dir: &Path, id: u64, issue_name: &str, msg: &ChatMsg) -> Result<()> {
    let dir = issues_dir(teamfly_dir);
    std::fs::create_dir_all(&dir)?;
    let path = issue_path(teamfly_dir, id, issue_name);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开 {}", path.display()))?;
    // 一次 write 写完整行(含换行)。
    // `writeln!` 会拆成两个 write 系统调用(内容 + "\n"),而 O_APPEND 只保证
    // **单次** write 原子 —— 同一目录开两个 teamfly 实例时,两边的写会交错成
    // `{..A..}{..B..}\n\n`,重启时这一整行解析失败被跳过,两条消息一起消失。
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    f.write_all(line.as_bytes())
        .with_context(|| format!("写入 {}", path.display()))?;
    Ok(())
}

/// 议题改名时把落盘文件一起改名。源文件不存在视为成功(还没落过盘)。
/// 目标已存在则不覆盖 —— 那是另一个议题的历史,宁可留下孤儿也别冲掉它。
pub fn rename_file(teamfly_dir: &Path, id: u64, from: &str, to: &str) -> Result<()> {
    let src = issue_path(teamfly_dir, id, from);
    if !src.exists() {
        return Ok(());
    }
    let dst = issue_path(teamfly_dir, id, to);
    if dst.exists() {
        anyhow::bail!(
            "{} 已存在,不覆盖(旧文件 {} 保留待人工处理)",
            dst.display(),
            src.display()
        );
    }
    std::fs::rename(&src, &dst)
        .with_context(|| format!("把 {} 改名为 {}", src.display(), dst.display()))?;
    Ok(())
}

/// 删除议题的落盘文件(关闭议题时);文件不存在视为成功。
pub fn delete_file(teamfly_dir: &Path, id: u64, issue_name: &str) -> Result<()> {
    let path = issue_path(teamfly_dir, id, issue_name);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("删除 {}", path.display()))?;
    }
    Ok(())
}

/// 读回落盘的议题(重开恢复 tab 与时间线)。第二个返回值是需要提示给用户的告警(读不了的文件等) ——
/// 这些必须进 TUI 的预检消息,不能只往 stderr 打(马上就进备用屏了)。
pub fn load_all_issues(teamfly_dir: &Path) -> Result<(Vec<Issue>, Vec<String>)> {
    let dir = issues_dir(teamfly_dir);
    let mut warns: Vec<String> = Vec::new();
    if !dir.is_dir() {
        return Ok((Vec::new(), warns));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();

    let mut issues = Vec::new();
    // 旧格式(文件名无 <id>- 前缀)按加载顺序补发新 id,并改名到新格式,
    // 这样下次启动就稳定了。
    let mut legacy: Vec<(PathBuf, String)> = Vec::new();
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("issue")
            .to_string();
        let (id, name) = match parse_stem(&stem) {
            Some(v) => v,
            None => {
                // 旧格式:先记下来,读完内容后统一迁移
                legacy.push((path.clone(), stem.clone()));
                (0, stem.clone())
            }
        };
        // 按字节读 + lossy 转换:掉电/被 kill 时 jsonl 尾部常留半截甚至非法字节,
        // 以前 read_to_string 的 ? 会一路冒泡到 main,整个项目再也进不去 TUI,
        // 而且不告诉你是哪个文件。单行损坏本来就是跳过,这里保持一致。
        let content = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                // 不能只 eprintln:紧接着就 EnterAlternateScreen,用户永远看不到,
                // 只感觉某个 tab 凭空没了。攒起来交给调用方当预检 warn 显示。
                warns.push(format!("读不了议题文件 {}({e}),已跳过", path.display()));
                continue;
            }
        };
        let mut issue = if id == 0 {
            crate::model::Issue::new(name.clone()) // 旧格式:发个新 id
        } else {
            crate::model::issue_with_id(id, name.clone())
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMsg>(line) {
                issue.timeline.push(msg);
            }
        }
        issues.push(issue);
    }

    // 把旧格式文件迁到 <id>-<名字>.jsonl。迁移失败只警告,不影响本次运行
    //(内存里已经读进来了),但下次启动它还是会拿到新 id。
    for (old_path, name) in legacy {
        if let Some(issue) = issues.iter().find(|i| i.name == name) {
            let new_path = issue_path(teamfly_dir, issue.id, &name);
            if let Err(e) = std::fs::rename(&old_path, &new_path) {
                warns.push(format!(
                    "议题文件 {} 迁移到新命名失败({e});重启后 id 可能变",
                    old_path.display()
                ));
            }
        }
    }
    Ok((issues, warns))
}

/// 为被唤醒的 agent 拼「自上次活跃以来新增的群聊前情」+ 本次指派。
///
/// - `issue_id`:这一轮所属议题(增量前情按议题分别记账)
/// - `timeline`:当前 issue 的完整群聊时间线
/// - `member`:被唤醒者(按 issue_id 读它的 last_seen 算增量)
/// - `assignment`:本次投递给它的指派原文(来自 @ 或我)
pub fn build_prompt_input(
    issue_id: u64,
    timeline: &[ChatMsg],
    member: &Member,
    assignment: &str,
) -> String {
    let start = member.last_seen_for(issue_id).min(timeline.len());
    let recent = &timeline[start..];

    // 前情从**最近的**往前收,收满就停:prompt 是作为单个 argv 传给子进程的,
    // Linux 的 MAX_ARG_STRLEN 是 128KiB,超了直接 spawn 失败(E2BIG),
    // 群聊里只会看到一句「起 claude 失败: Argument list too long」,无从下手。
    let mut kept: Vec<String> = Vec::new();
    let mut budget = CONTEXT_MAX_CHARS;
    let mut dropped = 0usize;
    for m in recent.iter().rev() {
        let who = if m.is_system { "系统" } else { &m.author };
        let line = format!("{who}: {}\n", m.text);
        if line.chars().count() > budget {
            dropped = recent.len() - kept.len();
            break;
        }
        budget -= line.chars().count();
        kept.push(line);
    }
    kept.reverse();

    let mut s = String::new();
    if !kept.is_empty() || dropped > 0 {
        s.push_str("[团队新进展]\n");
        if dropped > 0 {
            s.push_str(&format!("(前面还有 {dropped} 条更早的消息,因过长被省略)\n"));
        }
        for line in &kept {
            s.push_str(line);
        }
        s.push_str("---\n");
    }
    s.push_str("现在轮到你:\n");
    // 指派本身也可能超长(上游把大段文件内容写进了汇报)
    s.push_str(&clamp_chars(assignment, ASSIGNMENT_MAX_CHARS));
    s.push_str(HANDOFF_NOTE);
    s
}

/// 前情部分最多占多少字符(留足余量给 system prompt 与指派)。
const CONTEXT_MAX_CHARS: usize = 24_000;
/// 单条指派最多占多少字符。
const ASSIGNMENT_MAX_CHARS: usize = 12_000;

/// 每次派活都追加的收尾说明。
///
/// 措辞要和 agents/*.md 的人设一致:DEV/REV 的人设要求「完成后 @TPM 汇报」,
/// 所以这里不能说「任务已完成就不要 @任何人」—— 那句话在 user 消息里,
/// 比 system prompt 更近更强,DEV 干完活会照它执行,于是 TPM 永不被唤醒、
/// REV 永不评审,界面上所有人都摸鱼,看起来像「做完了」。
const HANDOFF_NOTE: &str = "\n\n（干完后,用简短一段话总结你做了什么、结果如何。\
按你的职责决定是否接力:需要别人接手或需要向调度者汇报时,在结尾 @对应成员;\
如果这一轮是直接回答用户、不需要任何人接手,就不要 @任何成员。\
改过文件的话,顺手 `git add` + `git commit` 提交到当前分支 —— 接力的队友和你在同一个工作树里,\
不提交也看得到你的改动,但提交了负责人采纳这个议题时才拿得到完整历史。）";

fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n(…本条过长,已截断)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentState, BackendKind};
    use std::collections::VecDeque;

    fn member(seen: usize) -> Member {
        Member {
            name: "小盾".into(),
            role: "安全".into(),
            emoji: "🛡".into(),
            backend: BackendKind::Claude,
            model: None,
            read_only: false,
            system_prompt: String::new(),
            state: AgentState::Idle,
            inbox: VecDeque::new(),
            working_issue: None,
            raw: VecDeque::new(),
            last_seen: std::collections::HashMap::from([(TEST_ISSUE, seen)]),
        }
    }

    /// 测试用的固定议题 id
    const TEST_ISSUE: u64 = 7;

    fn msg(author: &str, text: &str) -> ChatMsg {
        ChatMsg {
            ts: "t".into(),
            author: author.into(),
            text: text.into(),
            is_system: false,
        }
    }

    #[test]
    fn parse_stem_splits_id_and_name() {
        assert_eq!(parse_stem("3-改登录"), Some((3, "改登录".to_string())));
        assert_eq!(parse_stem("12-fix-the-bug"), Some((12, "fix-the-bug".to_string())));
        // 旧格式(无 id 前缀)
        assert_eq!(parse_stem("改登录"), None);
        assert_eq!(parse_stem("not-a-number"), None);
    }

    /// id 必须跨重启稳定 —— worktree 目录和分支都按它命名。
    ///
    /// 以前 id 不落盘、文件按名字存,重启后 id 从 1 重排:议题会去找不属于它的
    /// worktree(自己的改动成孤儿),甚至复用到别的议题留下的那个,两边改动混一起。
    #[test]
    fn ids_survive_restart_and_legacy_files_migrate() {
        let dir = std::env::temp_dir().join(format!("tf_id_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();

        // 新格式:id 写在文件名里
        append_chat(&dir, 7, "改登录", &msg("我", "A")).unwrap();
        append_chat(&dir, 9, "查bug", &msg("我", "B")).unwrap();
        // 旧格式:没有 id 前缀
        std::fs::write(
            issues_dir(&dir).join("老议题.jsonl"),
            format!("{}\n", serde_json::to_string(&msg("我", "C")).unwrap()),
        )
        .unwrap();

        let (issues, _warns) = load_all_issues(&dir).unwrap();
        let by_name = |n: &str| issues.iter().find(|i| i.name == n).expect(n).id;

        // 新格式的 id 必须原样读回来,不能重排
        assert_eq!(by_name("改登录"), 7);
        assert_eq!(by_name("查bug"), 9);

        // 旧格式补了个新 id,而且必须避开已用的(不能撞上 7 或 9)
        let legacy_id = by_name("老议题");
        assert!(legacy_id != 7 && legacy_id != 9, "补的 id 撞了: {legacy_id}");

        // 旧文件已迁到新命名,下次启动就稳定了
        assert!(
            issues_dir(&dir).join(format!("{legacy_id}-老议题.jsonl")).exists(),
            "旧格式文件没被迁移"
        );
        assert!(!issues_dir(&dir).join("老议题.jsonl").exists());

        // 再加载一次:所有 id 都不变
        let (again, _) = load_all_issues(&dir).unwrap();
        for i in &issues {
            let same = again.iter().find(|x| x.name == i.name).unwrap();
            assert_eq!(same.id, i.id, "{} 的 id 重启后变了", i.name);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_context() {
        let tl = vec![
            msg("我", "把 auth 抽出来"),
            msg("老K", "拆三块 @小盾 接②"),
        ];
        let input = build_prompt_input(TEST_ISSUE, &tl, &member(1), "[来自 老K] 接②限流");
        // 只应含第 2 条(增量),不含第 1 条
        assert!(input.contains("拆三块"));
        assert!(!input.contains("把 auth 抽出来"));
        assert!(input.contains("接②限流"));
        assert!(input.contains("总结你做了什么"));
    }

    #[test]
    fn no_recent_when_caught_up() {
        let tl = vec![msg("我", "x")];
        let input = build_prompt_input(TEST_ISSUE, &tl, &member(1), "干活");
        assert!(!input.contains("[团队新进展]"));
    }
}

