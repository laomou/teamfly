//! issue 落盘与增量前情:jsonl 追加/重放,拼装被唤醒 agent 的群聊前情。

use crate::model::{ChatMsg, Issue, Member};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// issues 目录:<teamfly_dir>/issues
pub fn issues_dir(teamfly_dir: &Path) -> PathBuf {
    teamfly_dir.join("issues")
}

fn issue_path(teamfly_dir: &Path, name: &str) -> PathBuf {
    issues_dir(teamfly_dir).join(format!("{name}.jsonl"))
}

/// 追加一条群聊消息到落盘文件。
pub fn append_chat(teamfly_dir: &Path, issue_name: &str, msg: &ChatMsg) -> Result<()> {
    let dir = issues_dir(teamfly_dir);
    std::fs::create_dir_all(&dir)?;
    let path = issue_path(teamfly_dir, issue_name);
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
pub fn rename_file(teamfly_dir: &Path, from: &str, to: &str) -> Result<()> {
    let src = issue_path(teamfly_dir, from);
    if !src.exists() {
        return Ok(());
    }
    let dst = issue_path(teamfly_dir, to);
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
pub fn delete_file(teamfly_dir: &Path, issue_name: &str) -> Result<()> {
    let path = issue_path(teamfly_dir, issue_name);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("删除 {}", path.display()))?;
    }
    Ok(())
}

/// 从盘上重放所有 issue(重开恢复 tab 与时间线)。
/// 读回落盘的议题。第二个返回值是需要提示给用户的告警(读不了的文件等) ——
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
    for path in files {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("issue")
            .to_string();
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
        let mut issue = Issue::new(name);
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
注意:你改过的文件需要先 `git add` 并 `git commit` 提交到当前分支,这样接力的队友才能看到你的改动。）";

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
            system_prompt: String::new(),
            state: AgentState::Idle,
            inbox: VecDeque::new(),
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

