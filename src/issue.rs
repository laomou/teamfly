//! issue 落盘与增量前情:jsonl 追加/重放,拼装被唤醒 agent 的群聊前情。

use crate::model::{ChatMsg, Issue, Member};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// issues 目录:<teamfly_dir>/issues
fn issues_dir(teamfly_dir: &Path) -> PathBuf {
    teamfly_dir.join("issues")
}

/// 已发放 id 的水位线文件:`<teamfly_dir>/next-issue-id`。
///
/// 为什么必须落盘:`NEXT_ISSUE_ID` 以前只从**存活的** jsonl 文件推高
/// (`fetch_max(id+1)`),而关议题会删掉 jsonl、却**故意保留分支**
/// (关掉只是「我不看了」,不该销毁工作成果)。于是关掉 id 最大的那个议题
/// 之后重启,新议题会拿回同一个 id,`prepare` 撞上 `teamfly/issue-<id>`
/// 已存在 —— 新议题的 agent 站在上一个议题的成果上干活,或者(更糟)
/// 那个分支被当成本议题的、在 `drop_if_untouched` 时被 `branch -D`。
fn watermark_path(teamfly_dir: &Path) -> PathBuf {
    teamfly_dir.join("next-issue-id")
}

/// 读水位线。文件不存在/读不动/内容不是数字都返回 0(退回「按存活文件推高」)。
fn read_watermark(teamfly_dir: &Path) -> u64 {
    std::fs::read_to_string(watermark_path(teamfly_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// 把水位线推到至少 `id`。失败返回 Err 让调用方决定怎么提示 ——
/// 写不进去的话下次启动 id 就会退回去,那正是这个文件要防的事。
pub fn bump_watermark(teamfly_dir: &Path, id: u64) -> Result<()> {
    if read_watermark(teamfly_dir) >= id {
        return Ok(());
    }
    std::fs::create_dir_all(teamfly_dir)?;
    let path = watermark_path(teamfly_dir);
    // 先写临时文件再 rename:写一半掉电不会留下半截数字被解析成小值
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{id}\n"))
        .with_context(|| format!("写入 {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("替换 {}", path.display()))?;
    Ok(())
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
    // 先把 id 计数器推到落盘的水位线之上 —— 必须在读文件**之前**,
    // 这样即使 issues/ 目录是空的(所有议题都关掉了),新议题也不会
    // 从 1 开始重发那些分支还在的 id。
    let mark = read_watermark(teamfly_dir);
    if mark > 0 {
        crate::model::reserve_issue_ids_up_to(mark);
    }
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
    // 记 (旧路径, 新发的 id, 名字):**不能**事后靠名字回查议题 ——
    // find(|i| i.name == name) 会命中第一个同名的,而那可能是个新格式议题,
    // 于是把它的文件当成迁移目标覆盖掉。
    let mut legacy: Vec<(PathBuf, u64, String)> = Vec::new();
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("issue")
            .to_string();
        let (id, name) = match parse_stem(&stem) {
            Some(v) => v,
            None => (0, stem.clone()), // 旧格式:下面发个新 id,并登记待迁移
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
        if id == 0 {
            // 记下**这个议题自己**的新 id,读完后迁移。不能事后按名字回查。
            legacy.push((path.clone(), issue.id, name.clone()));
        }
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
    for (old_path, id, name) in legacy {
        let new_path = issue_path(teamfly_dir, id, &name);
        // 目标已存在 → **绝不覆盖**。那是另一个议题的完整历史,而这个旧文件
        // 只是个同名的孤儿(第一次迁移失败后本次会话又建了新文件,或用户从
        // 备份恢复了一个旧名字的文件)。rename 会静默把它整个冲掉,warns 里
        // 一个字都没有。宁可留下孤儿也别丢历史 —— 和 rename_file 一致。
        if new_path.exists() {
            warns.push(format!(
                "{} 已存在,不迁移 {}(旧文件保留待人工处理,以免覆盖另一个议题的历史)",
                new_path.display(),
                old_path.display()
            ));
            continue;
        }
        if let Err(e) = std::fs::rename(&old_path, &new_path) {
            warns.push(format!(
                "议题文件 {} 迁移到新命名失败({e});重启后 id 可能变",
                old_path.display()
            ));
        }
    }
    // 把水位线补到「所有见过的 id」之上。
    //
    // 兼容老项目:它们没有这个文件,水位线要从现存文件重建。这补不回**已经
    // 关掉**的议题(那些 jsonl 已经没了),所以老项目升级后第一次仍可能撞上
    // 一次 id 回收 —— 但 prepare 会拒绝复用已存在的分支并 fallback,不会
    // 静默混改动。之后水位线就稳了。
    let high = issues.iter().map(|i| i.id).max().unwrap_or(0);
    if high > 0 {
        if let Err(e) = bump_watermark(teamfly_dir, high + 1) {
            warns.push(format!(
                "议题 id 水位线写不进去({e});关掉议题后重启可能重发已用过的 id,\
                 撞上残留分支时那一轮会退回主工作目录"
            ));
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
    //
    // 预算按**字节**算,不能按字符 —— 这个项目的主要语言是中文,一个汉字
    // 3 字节、emoji 4 字节。按字符算的话对英文留了 3-4 倍余量,对中文只留 1 倍,
    // 正好在最常见的场景下失效。
    let mut kept: Vec<String> = Vec::new();
    let mut budget = CONTEXT_MAX_BYTES;
    let mut dropped = 0usize;
    for m in recent.iter().rev() {
        let who = if m.is_system { "系统" } else { &m.author };
        let line = format!("{who}: {}\n", m.text);
        if line.len() > budget {
            dropped = recent.len() - kept.len();
            break;
        }
        budget -= line.len();
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
    s.push_str(&clamp_bytes(assignment, ASSIGNMENT_MAX_BYTES));
    s.push_str(HANDOFF_NOTE);
    s
}

/// 前情部分最多占多少**字节**(留足余量给 system prompt 与指派)。
///
/// 单个 argv 的硬上限是 `MAX_ARG_STRLEN` = 128 KiB(实测 131072 字节整)。
/// codex 后端把 system_prompt 和这段拼进**同一个** argv,所以这两个预算
/// 加起来还要给 system_prompt 留出空间。
const CONTEXT_MAX_BYTES: usize = 48_000;
/// 单条指派最多占多少字节。
const ASSIGNMENT_MAX_BYTES: usize = 24_000;

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

/// 按**字节**截断,但不切断 UTF-8 字符边界(切断了 String 就构造不出来)。
fn clamp_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // 从 max 往前退到最近的字符边界
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
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

    /// 关掉议题后,水位线必须保住已发放的最大 id。
    ///
    /// 关议题会删 jsonl 但**故意保留分支**。以前 id 计数器只从存活的 jsonl
    /// 推高,于是关掉 id 最大的那个之后重启,新议题会拿回同一个 id,
    /// `prepare` 撞上还在的 `teamfly/issue-<id>`。
    ///
    /// 这里不测 `Issue::new()` 的返回值 —— `NEXT_ISSUE_ID` 是进程全局的,
    /// 同一个测试二进制里其他测试已经把它推高了,测绝对值没有意义。
    /// 改为验证水位线文件的内容:它是跨进程持久化的,才是真正的修复点。
    #[test]
    fn watermark_written_after_load() {
        let dir = std::env::temp_dir().join(format!("tf_ww_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();
        for (id, n) in [(1u64, "a"), (5, "b"), (3, "c")] {
            append_chat(&dir, id, n, &msg("我", "x")).unwrap();
        }
        // 加载前水位线不存在
        assert!(!dir.join("next-issue-id").exists(), "加载前不该有水位线文件");

        let _ = load_all_issues(&dir).unwrap();

        // 加载后水位线必须存在且 >= max_id + 1
        let wm = read_watermark(&dir);
        assert!(wm >= 6, "水位线 {wm} 应 >= 6(max id=5, next=6)");
    }

    /// 关掉 id 最大的议题后,水位线必须保住那个 id。
    #[test]
    fn watermark_survives_issue_close() {
        let dir = std::env::temp_dir().join(format!("tf_ws_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();
        for (id, n) in [(1u64, "a"), (2, "b"), (7, "c")] {
            append_chat(&dir, id, n, &msg("我", "x")).unwrap();
        }
        let _ = load_all_issues(&dir).unwrap();
        let wm_before = read_watermark(&dir);
        assert!(wm_before >= 8, "初次加载后水位线 {wm_before} 应 >= 8");

        // 关掉 id 最大的那个
        delete_file(&dir, 7, "c").unwrap();

        // 重启:重新加载
        let _ = load_all_issues(&dir).unwrap();
        let wm_after = read_watermark(&dir);
        assert!(
            wm_after >= 8,
            "关掉 id=7 的议题后水位线从 {wm_before} 退到了 {wm_after} ——              重启后新议题会拿回 id 7,而 teamfly/issue-7 还在盘上"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 所有议题都关掉后重启,水位线仍然保住。
    #[test]
    fn watermark_survives_empty_issues_dir() {
        let dir = std::env::temp_dir().join(format!("tf_wm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();
        append_chat(&dir, 5, "唯一的议题", &msg("我", "x")).unwrap();
        let _ = load_all_issues(&dir).unwrap();
        let wm_before = read_watermark(&dir);
        assert!(wm_before >= 6);

        delete_file(&dir, 5, "唯一的议题").unwrap();
        let (empty, _) = load_all_issues(&dir).unwrap();
        assert!(empty.is_empty());

        let wm_after = read_watermark(&dir);
        assert!(
            wm_after >= 6,
            "issues/ 空了之后水位线从 {wm_before} 退到了 {wm_after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 水位线文件损坏(半截数字/乱码)不能让 id 退回去,也不能崩。
    #[test]
    fn corrupt_watermark_falls_back_to_files() {
        let dir = std::env::temp_dir().join(format!("tf_wmc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();
        append_chat(&dir, 9, "议题", &msg("我", "x")).unwrap();
        std::fs::write(dir.join("next-issue-id"), "这不是数字\n").unwrap();

        let (issues, _) = load_all_issues(&dir).unwrap();
        assert_eq!(issues[0].id, 9, "文件名里的 id 该照常读出来");
        // 损坏时退回按存活文件推高,水位线应被重建
        let wm = read_watermark(&dir);
        assert!(wm >= 10, "损坏后水位线应被重建为 >= 10,实际 {wm}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧格式迁移**绝不能覆盖**已存在的文件。
    ///
    /// 两个 bug 叠在一起才会踩到:
    /// 1. 迁移目标是按名字回查议题算出来的(`find(|i| i.name == name)`),
    ///    命中的可能是另一个**同名**议题,于是目标指向它的文件;
    /// 2. `fs::rename` 没有 `dst.exists()` 保护(`rename_file` 早就有)。
    ///
    /// 结果是几十条历史被静默冲成旧文件那一条,warns 里一个字都没有。
    #[test]
    fn legacy_migration_never_overwrites_existing_history() {
        let dir = std::env::temp_dir().join(format!("tf_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();

        // 新格式议题「改登录」,3 条历史
        for t in ["第一条", "第二条", "第三条"] {
            append_chat(&dir, 7, "改登录", &msg("我", t)).unwrap();
        }
        // 同名的旧格式孤儿文件,只有 1 条
        std::fs::write(
            issues_dir(&dir).join("改登录.jsonl"),
            format!("{}\n", serde_json::to_string(&msg("我", "孤儿那条")).unwrap()),
        ).unwrap();

        let (issues, _warns) = load_all_issues(&dir).unwrap();

        // 新格式那个议题的历史必须完好 —— 内存里和盘上都是
        let kept = issues.iter().find(|i| i.id == 7).expect("id=7 的议题没了");
        assert_eq!(kept.timeline.len(), 3, "新格式议题的历史被旧文件覆盖了");
        let on_disk =
            std::fs::read_to_string(issues_dir(&dir).join("7-改登录.jsonl")).unwrap();
        assert_eq!(
            on_disk.lines().filter(|l| !l.trim().is_empty()).count(),
            3,
            "盘上的历史被覆盖了"
        );
        // 孤儿也没丢:它拿到自己的新 id,迁到自己的文件里
        assert!(
            issues.iter().any(|i| i.id != 7 && i.timeline.len() == 1),
            "孤儿文件的那条消息丢了"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迁移目标真的已存在时(比如上次迁移失败留下的同 id 文件),
    /// 必须**保留旧文件并告警**,不能静默 rename 覆盖。
    #[test]
    fn legacy_migration_warns_instead_of_clobbering() {
        let dir = std::env::temp_dir().join(format!("tf_mig2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(issues_dir(&dir)).unwrap();

        // 先让一个旧格式文件跑一遍迁移,拿到它的新 id
        std::fs::write(
            issues_dir(&dir).join("待迁移.jsonl"),
            format!("{}\n", serde_json::to_string(&msg("我", "原始那条")).unwrap()),
        ).unwrap();
        let (first, _) = load_all_issues(&dir).unwrap();
        let assigned = first[0].id;
        assert!(
            issues_dir(&dir).join(format!("{assigned}-待迁移.jsonl")).exists(),
            "第一次该迁过去"
        );

        // 现在人为再放一个同名旧格式文件回去,并让它会算出同一个目标
        // (模拟:第一次迁移失败后本次会话又建了新文件,或从备份恢复)
        std::fs::write(
            issues_dir(&dir).join("待迁移.jsonl"),
            format!("{}\n", serde_json::to_string(&msg("我", "后来那条")).unwrap()),
        ).unwrap();
        // 把新格式文件的 id 改成「下一个会被发出去的 id」,制造真实碰撞
        let next = assigned + 1;
        std::fs::rename(
            issues_dir(&dir).join(format!("{assigned}-待迁移.jsonl")),
            issues_dir(&dir).join(format!("{next}-待迁移.jsonl")),
        ).unwrap();

        let before: Vec<String> = std::fs::read_to_string(
            issues_dir(&dir).join(format!("{next}-待迁移.jsonl")),
        ).unwrap().lines().map(String::from).collect();

        let (_issues, warns) = load_all_issues(&dir).unwrap();

        let after: Vec<String> = std::fs::read_to_string(
            issues_dir(&dir).join(format!("{next}-待迁移.jsonl")),
        ).unwrap().lines().map(String::from).collect();
        assert_eq!(before, after, "已存在的文件被覆盖了");
        // 撞了就必须留痕,不能悄悄跳过
        if issues_dir(&dir).join("待迁移.jsonl").exists() {
            assert!(
                warns.iter().any(|w| w.contains("不迁移") || w.contains("已存在")),
                "旧文件留下了但没告警: {warns:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
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

