//! backend 驱动:无状态每轮重起。喂 (system_prompt, user_input) → 流式产出 raw 行 → 结束汇总。
//! 只支持 claude(stream-json)与 codex(纯文本)两个子进程 backend。

use crate::model::{BackendKind, Msg};
use crate::router::strip_ansi;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub struct RunSpec {
    pub name: String,
    /// 这一轮属于哪个议题(Issue::id),原样回投给 AgentDone
    pub issue: u64,
    /// 派活时的团队代号,原样回投给 AgentDone
    pub gen: u64,
    /// 派活时的取消纪元,原样回投给 AgentDone
    pub cancel_gen: u64,
    pub backend: BackendKind,
    /// frontmatter 指定的模型;None = 不传 --model,由 CLI 自己决定。
    pub model: Option<String>,
    pub system_prompt: String,
    pub user_input: String,
    pub work_dir: PathBuf,
    /// 预解析好的 MCP 配置文件路径(查的是主 work_dir,不是 worktree)
    pub mcp_config: Option<String>,
    /// 这一轮的 worktree (目录, 分支);fallback 时为 None。原样回投给 AgentDone。
    pub worktree: Option<(PathBuf, String)>,
    /// 只读:不给写权限。用于 `read_only: true` 的成员 ——
    /// 它们直接在**用户的主工作树**里跑,一旦能写就会污染用户工作区
    /// (最典型的是照着接力说明去 `git merge` 上游分支,把改动并进用户主分支,
    ///  绕掉「改动不自动进主分支、用户审批才 merge」这个核心保证)。
    pub read_only: bool,
}

/// 重试前追加给 agent 的提醒。上一次尝试已经动过工具(可能改了文件、跑过命令),
/// 把同一个 prompt 原样再跑一遍会重复副作用(重复改动、重复 commit),
/// 所以必须先让它自己核对现状。
const RETRY_NOTE: &str = "\n\n[系统提醒] 上一次尝试中途失败了,而且当时你已经动过工具(读写文件或跑命令),\
工作区可能已被部分修改。请先核对当前实际状态(比如 git status / git diff、读一遍相关文件),\
在已完成的部分之上继续,不要从零重做,更不要重复提交。";

/// 起一个 agent 干一轮活。流式把每行 raw 通过 tx 回投,结束回投 AgentDone。
/// 本函数应在 tokio task 中调用。失败自动重试(应对中转站 429/5xx 等瞬时错误)。
/// `cancel` 用于外部取消(用户按 Ctrl+C 或退出),收到后 kill 子进程且不再重试。
pub async fn run(spec: RunSpec, cancel: CancellationToken, tx: UnboundedSender<Msg>) {
    let name = spec.name.clone();
    let issue = spec.issue;
    let gen = spec.gen;
    let worktree = spec.worktree.clone();
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();
    // 之前的尝试有没有动过工具 —— 决定重试时要不要带上核对提醒
    let mut touched_tools = false;

    for attempt in 1..=MAX_ATTEMPTS {
        if cancel.is_cancelled() {
            let _ = tx.send(Msg::AgentDone {
                name,
                issue,
                gen,
                cancel_gen: spec.cancel_gen,
                worktree: worktree.clone(),
                full_output: String::new(),
                ok: false,
                err: Some(crate::model::CANCELLED.into()),
            });
            return;
        }
        // 重试且上次动过工具:prompt 里补一段「先核对现状」的提醒
        let effective_input = if attempt > 1 && touched_tools {
            format!("{}{}", spec.user_input, RETRY_NOTE)
        } else {
            spec.user_input.clone()
        };
        let mut used_tools_this_attempt = false;
        let result = match spec.backend {
            BackendKind::Claude => {
                run_process(
                    &spec,
                    &tx,
                    claude_cmd(&spec, &effective_input),
                    crate::stream::StreamFmt::Claude,
                    &cancel,
                    &mut used_tools_this_attempt,
                )
                .await
            }
            BackendKind::Codex => {
                run_process(
                    &spec,
                    &tx,
                    codex_cmd(&spec, &effective_input),
                    crate::stream::StreamFmt::Codex,
                    &cancel,
                    &mut used_tools_this_attempt,
                )
                .await
            }
        };
        touched_tools |= used_tools_this_attempt;

        match result {
            Ok(full) => {
                let _ = tx.send(Msg::AgentDone {
                    name,
                    issue,
                    gen,
                    cancel_gen: spec.cancel_gen,
                    worktree: worktree.clone(),
                    full_output: full,
                    ok: true,
                    err: None,
                });
                return;
            }
            Err(e) => {
                last_err = e.to_string();
                // 取消导致的失败:不重试、不报「重试 N 次仍失败」
                if cancel.is_cancelled() {
                    let _ = tx.send(Msg::AgentDone {
                        name,
                        issue,
                        gen,
                        cancel_gen: spec.cancel_gen,
                        worktree: worktree.clone(),
                        full_output: String::new(),
                        ok: false,
                        err: Some(crate::model::CANCELLED.into()),
                    });
                    return;
                }
                if attempt < MAX_ATTEMPTS {
                    // 提示这次失败要重试,并退避。动过工具时明说重试会带核对提醒,
                    // 因为这时候的重跑不是「白跑一遍」,用户有权知道。
                    let hint = if touched_tools {
                        format!("⟨err⟩ 第{attempt}次失败(已动过工具,重试会先让它核对现状):{last_err}")
                    } else {
                        format!("⟨err⟩ 第{attempt}次失败,重试中… {last_err}")
                    };
                    let _ = tx.send(Msg::AgentStdout { name: name.clone(), line: hint });
                    tokio::time::sleep(std::time::Duration::from_millis(600 * attempt as u64)).await;
                } else {
                    // 最后一次也失败,打印后走下面的掉线
                    let _ = tx.send(Msg::AgentStdout {
                        name: name.clone(),
                        line: format!("⟨err⟩ 第{attempt}次失败(已达上限):{last_err}"),
                    });
                }
            }
        }
    }

    // 重试用尽,报掉线。动过工具就明说工作区可能已被改过 ——
    // 以前只说「掉线」,用户根本不知道仓库已经被动了。
    let tail = if touched_tools {
        ";期间它动过工具,工作区可能已被部分修改,建议 git status 看一眼"
    } else {
        ""
    };
    let _ = tx.send(Msg::AgentDone {
        name,
        issue,
        gen,
        cancel_gen: spec.cancel_gen,
        worktree,
        full_output: String::new(),
        ok: false,
        err: Some(format!("重试 {MAX_ATTEMPTS} 次仍失败:{last_err}{tail}")),
    });
}

/// 构造 claude CLI 命令(headless stream-json + bypass 权限 + 禁反问 + 追加系统 prompt)。
fn claude_cmd(spec: &RunSpec, user_input: &str) -> ProcSpec {
    // 只读成员用 plan 模式:实测它挡得住写(连 Bash 里的 shell 重定向也挡),
    // 但读文件、跑 git diff / git log 都正常 —— 正好是评审/调度需要的。
    // 只禁 Edit/Write 是挡不住的:agent 会直接用 Bash 写进去(实测验证过)。
    let mode = if spec.read_only { "plan" } else { "bypassPermissions" };
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(), // stream-json 必需
        "--permission-mode".to_string(),
        mode.to_string(),
        "--disallowedTools".to_string(),
        "AskUserQuestion".to_string(), // 禁 agent 反问(没有人在终端那头等着答)
        "--append-system-prompt".to_string(),
        spec.system_prompt.clone(),
    ];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    // MCP 配置已在 spawn 时从主 work_dir 解析好(worktree 里没有 .teamfly/)
    if let Some(mcp) = &spec.mcp_config {
        args.push("--mcp-config".into());
        args.push(mcp.clone());
        args.push("--strict-mcp-config".into());
    }
    args.push(clamp_argv(user_input));
    ProcSpec {
        bin: "claude".into(),
        args,
    }
}

/// 单个 argv 的字节上限。`MAX_ARG_STRLEN` 实测正好是 131072 字节
/// (131071 可以,131072 就 E2BIG),这里留 4 KiB 余量。
const ARGV_MAX_BYTES: usize = 127 * 1024;

/// 把一个即将成为 argv 的字符串按字节封顶,不切断 UTF-8 字符边界。
///
/// 这是 E2BIG 的最后一道防线:超了的话 spawn 直接失败,群聊里只有一句
/// 「起 codex 失败: Argument list too long」,而且这个议题此后每次派活都会
/// 失败 —— 前情只会越来越长,自己不会恢复。
fn clamp_argv(s: &str) -> String {
    if s.len() <= ARGV_MAX_BYTES {
        return s.to_string();
    }
    // 保**尾部**:最近的消息和本次指派在后面,比更早的前情重要
    let mut start = s.len() - ARGV_MAX_BYTES;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("(⚠ 前情过长,开头已被截断)\n{}", &s[start..])
}

/// MCP 配置文件:项目级优先,回退到用户级。都不存在返回 None。
pub fn resolve_mcp_config(work_dir: &std::path::Path) -> Option<String> {
    // 项目级
    let proj = work_dir.join(".teamfly").join("mcp.json");
    if proj.is_file() {
        return Some(proj.to_string_lossy().into_owned());
    }
    // 用户级
    let home = std::env::var_os("HOME")?;
    let user = std::path::PathBuf::from(home).join(".teamfly").join("mcp.json");
    if user.is_file() {
        Some(user.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// 构造 codex CLI 非交互命令(JSONL 事件流 + 跳过 git/sandbox 检查)。
fn codex_cmd(spec: &RunSpec, user_input: &str) -> ProcSpec {
    // codex 把 system_prompt 和 user_input 拼进**同一个** argv(claude 是分开传的),
    // 所以这里是全项目唯一知道最终 argv 到底多大的地方 —— 必须兜底封顶。
    // 上游 build_prompt_input 已按字节留了余量,但 system_prompt 完全不在它的
    // 预算里(团队自定义人设可以写得很长),重试时 RETRY_NOTE 还会再往外顶。
    let combined = clamp_argv(&format!("{}\n\n{}", spec.system_prompt, user_input));
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),                // JSONL 事件流
        "--skip-git-repo-check".to_string(), // 不要求工作目录是 git 库
    ];
    if spec.read_only {
        args.push("--sandbox".to_string());
        args.push("read-only".to_string());
    } else {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    }
    if let Some(m) = &spec.model {
        args.push("--model".to_string());
        args.push(m.clone());
    }
    args.push(combined);
    ProcSpec {
        bin: "codex".into(),
        args,
    }
}

/// 杀掉 agent **及它起的所有孙子进程**。
///
/// `child.kill()` 只发 SIGKILL 给直接子进程;agent 用 Bash 起的
/// cargo build / npm test 会活下来继续改工作区。因为 spawn 时给了
/// `process_group(0)`,这里可以按进程组一次杀干净。
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // 负号 = 整个进程组。失败(组已空/已回收)就无所谓,下面还有兜底。
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
    }
    let _ = child.kill().await;
}

struct ProcSpec {
    bin: String,
    args: Vec<String>,
}

async fn run_process(
    spec: &RunSpec,
    tx: &UnboundedSender<Msg>,
    proc: ProcSpec,
    fmt: crate::stream::StreamFmt,
    cancel: &CancellationToken,
    // 出参:这一轮 agent 有没有动过工具(决定重试要不要带核对提醒)
    used_tools: &mut bool,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(&proc.bin);
    cmd.args(&proc.args)
        .current_dir(&spec.work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 兜底:任何提前 return / panic / 进程退出都别把子进程留成孤儿。
        // 这些 agent 跑的是 bypassPermissions,留下来会继续改工作区。
        .kill_on_drop(true);
    // 让 agent 自成一个进程组,这样取消时能连它起的**孙子进程**一起杀。
    //
    // agent 用 Bash 工具起的 cargo build / npm test / 脚本都是孙子进程。
    // 只 kill 直接子进程的话它们全都活下来继续写文件 —— 实测过:
    // 杀掉 agent 后孙子进程 1.2s 内又往文件里追加了 6 行。
    // 用户看到「已取消 N 个在跑的 agent」,而仓库还在被改。
    #[cfg(unix)]
    cmd.process_group(0);


    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("起 `{}` 失败: {e}", proc.bin))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let mut full = String::new();       // 累积 assistant 文本
    let mut result_text: Option<String> = None; // 最终 result 文本
    let mut stream_err: Option<String> = None;  // 流里的**致命**错误
    let mut last_warn: Option<String> = None;   // 流里最后一条**瞬时**错误(仅兜底用)
    let mut err_tail: Vec<String> = Vec::new(); // 保留最后几行 stderr 供报错
    // 已经提示过「模型没返回思考内容」了吗?一轮里只说一次 ——
    // thinking 块可能有很多个,每个都提示会把 raw 视图刷满。
    let mut empty_thinking_noted = false;
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            biased; // 优先响应取消
            _ = cancel.cancelled() => {
                kill_tree(&mut child).await;
                anyhow::bail!("{}", crate::model::CANCELLED);
            }
            line = out_reader.next_line() => {
                match line? {
                    Some(l) => {
                        let clean = strip_ansi(&l);
                        let outcome = crate::stream::classify(fmt, &clean);
                        for disp in outcome.display {
                            let _ = tx.send(Msg::AgentStdout { name: spec.name.clone(), line: disp });
                        }
                        // 有 thinking 块但内容是空的(中转站剥掉了正文)。不说的话
                        // 用户分不清是模型没思考、中转站剥了、还是 teamfly 坏了 ——
                        // 只看到「从来没有 💭」。
                        if outcome.empty_thinking && !empty_thinking_noted {
                            empty_thinking_noted = true;
                            let _ = tx.send(Msg::AgentStdout {
                                name: spec.name.clone(),
                                line: "💭 (该模型未返回思考内容)".to_string(),
                            });
                        }
                        if let Some(t) = outcome.text_delta {
                            full.push_str(&t);
                            full.push('\n');
                        }
                        if outcome.tool_used {
                            *used_tools = true;
                        }
                        if let Some(w) = outcome.warn {
                            last_warn = Some(w);
                        }
                        if let Some(e) = outcome.error {
                            stream_err = Some(e);
                        }
                        if let Some(r) = outcome.result {
                            result_text = Some(r);
                            // 拿到结果 = 之前那些错误已经被恢复了,别再判整轮失败
                            stream_err = None;
                        }
                    }
                    None => break,
                }
            }
            line = err_reader.next_line() => {
                if let Some(l) = line? {
                    let clean = strip_ansi(&l);
                    push_tail(&mut err_tail, &clean);
                    let _ = tx.send(Msg::AgentStdout { name: spec.name.clone(), line: format!("⟨err⟩ {clean}") });
                }
            }
        }
    }
    // 排空 stderr 剩余
    while let Some(l) = err_reader.next_line().await? {
        let clean = strip_ansi(&l);
        push_tail(&mut err_tail, &clean);
        let _ = tx.send(Msg::AgentStdout {
            name: spec.name.clone(),
            line: format!("⟨err⟩ {clean}"),
        });
    }

    let status = child.wait().await?;

    // 流里的致命错误(claude 的 result.is_error / codex 的 turn.failed)视为失败,即便退出码 0
    if let Some(e) = stream_err {
        anyhow::bail!("{} 报错:{e}", proc.bin);
    }

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());
        let detail = err_tail
            .iter()
            .filter(|l| !l.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" / ");
        if detail.is_empty() {
            anyhow::bail!("{} 退出码 {code}(无 stderr)", proc.bin);
        } else {
            anyhow::bail!("{} 退出码 {code}:{detail}", proc.bin);
        }
    }
    // 退出码 0 但一个字都没产出:此时若流里有瞬时错误,它才是真正的失败原因。
    // (否则会把「(无输出)」当成正式汇报入群聊,把真失败洗成成功。)
    if result_text.is_none() && full.trim().is_empty() {
        if let Some(w) = last_warn {
            anyhow::bail!("{} 无输出,最后的错误:{w}", proc.bin);
        }
    }
    // stream-json:优先用 result 事件的最终文本;否则回退到累积的 assistant 文本
    Ok(result_text.unwrap_or(full))
}

/// 只保留最后 8 行 stderr。
fn push_tail(tail: &mut Vec<String>, line: &str) {
    tail.push(line.to_string());
    while tail.len() > 8 {
        tail.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BackendKind;

    /// frontmatter 的 `model:` 必须落成 `--model <名>`,而且要在**位置参数之前** ——
    /// claude 的 prompt / codex 的 combined 都是位置参数,插到它们后面会被当成
    /// prompt 的一部分,模型静默不生效。
    #[test]
    fn model_becomes_model_flag_before_positional() {
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut sp = spec(false, backend);
            sp.model = Some("some-model".into());
            let p = match backend {
                BackendKind::Claude => claude_cmd(&sp, "ui"),
                BackendKind::Codex => codex_cmd(&sp, "ui"),
            };
            let i = p.args.iter().position(|a| a == "--model").expect("该有 --model");
            assert_eq!(p.args[i + 1], "some-model");
            assert!(i + 2 < p.args.len(), "--model 不能是最后一对,位置参数得在它后面");
        }
    }

    /// 不指定就完全不传 —— 交给 CLI 自己按它的配置决定,teamfly 不替它选。
    #[test]
    fn no_model_flag_when_unset() {
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let sp = spec(false, backend);
            let p = match backend {
                BackendKind::Claude => claude_cmd(&sp, "ui"),
                BackendKind::Codex => codex_cmd(&sp, "ui"),
            };
            assert!(!p.args.iter().any(|a| a == "--model"));
        }
    }

    /// 取消必须连 agent 起的**孙子进程**一起杀。
    ///
    /// agent 用 Bash 工具起的 cargo build / npm test 都是孙子进程。只 kill
    /// 直接子进程的话它们全都活下来继续写文件 —— 用户看到「已取消 N 个在跑的
    /// agent」,而仓库还在被改。实测过:裸 kill 之后 1.2s 内又追加了 6 行。
    ///
    /// 这个测试起真实进程树来验,不是断言代码形状。
    #[tokio::test]
    async fn cancel_kills_grandchildren() {
        let dir = std::env::temp_dir().join(format!("tf_kg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("grandchild.log");

        // 父进程立刻退,只留孙子在后台写 —— 模拟 agent 起了个长跑命令
        let script = format!(
            "sh -c 'i=0; while [ $i -lt 200 ]; do echo x >> {} ; sleep 0.1; i=$((i+1)); done' & wait",
            out.display()
        );
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &script]).kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("起得来");

        // 等孙子真的开始写
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if out.exists() && std::fs::read(&out).map(|b| !b.is_empty()).unwrap_or(false) {
                break;
            }
        }
        let lines = || -> usize {
            std::fs::read_to_string(&out).map(|s| s.lines().count()).unwrap_or(0)
        };
        assert!(lines() > 0, "孙子进程没起来,这个测试就白测了");

        kill_tree(&mut child).await;
        let after_kill = lines();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(
            lines(), after_kill,
            "取消后孙子进程还在写文件(裸 kill 只杀得掉直接子进程)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E2BIG 的最后一道防线:任何一个 argv 都不能超过 MAX_ARG_STRLEN。
    ///
    /// 实测阈值正好 131072 字节(131071 可以,131072 就 `Argument list too long`)。
    /// 这个测试**真的去 spawn** 一个进程来验证 —— 光断言长度小于常量的话,
    /// 常量本身写错了就测不出来。
    #[test]
    fn no_argv_exceeds_e2big_limit() {
        // 最坏情况:超长 system_prompt(自定义团队) + 超长前情/指派,全中文
        let big_sp: String = "团队规约".repeat(6_000);   // ~72 KB
        let big_input: String = "汇报内容".repeat(20_000); // ~240 KB
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut sp = spec(false, backend);
            sp.system_prompt = big_sp.clone();
            let p = match backend {
                BackendKind::Claude => claude_cmd(&sp, &big_input),
                BackendKind::Codex => codex_cmd(&sp, &big_input),
            };
            for (i, a) in p.args.iter().enumerate() {
                assert!(
                    a.len() < 131_072,
                    "{backend:?} 的第 {i} 个 argv 有 {} 字节,会 E2BIG",
                    a.len()
                );
            }
            // 真的 spawn 一次(用 /bin/true,只验证内核收不收这组 argv)
            let r = std::process::Command::new("/bin/true").args(&p.args).output();
            assert!(r.is_ok(), "{backend:?} 的 argv 内核拒收: {:?}", r.err());
        }
    }

    /// 中文场景下 clamp 必须按字节生效 —— 按字符算的话 3 倍余量全没了。
    #[test]
    fn clamp_argv_counts_bytes_not_chars() {
        let cjk: String = "汉".repeat(60_000); // 60k 字符 = 180 KB
        assert!(cjk.chars().count() < 131_072, "按字符算会以为没超");
        assert!(cjk.len() > 131_072, "按字节算确实超了");
        let out = clamp_argv(&cjk);
        assert!(out.len() <= ARGV_MAX_BYTES + 64, "clamp 后还是 {} 字节", out.len());
        // 不能切断 UTF-8:能正常当 String 用就说明边界对了
        assert!(out.chars().count() > 0);
    }

    fn spec(read_only: bool, backend: BackendKind) -> RunSpec {
        RunSpec {
            name: "X".into(),
            issue: 1,
            gen: 0,
            cancel_gen: 0,
            backend,
            model: None,
            system_prompt: "sp".into(),
            user_input: "ui".into(),
            work_dir: PathBuf::from("/tmp"),
            mcp_config: None,
            worktree: None,
            read_only,
        }
    }

    /// `read_only: true` 的成员直接在**用户主工作树**里跑,必须真的没有写权限。
    /// 一旦能写,它照着接力说明去 `git merge` 上游分支就会把改动并进用户主分支,
    /// 绕掉「改动不自动进主分支、用户审批才 merge」这个核心保证。
    #[test]
    fn read_only_member_gets_no_write_permission() {
        let ro = claude_cmd(&spec(true, BackendKind::Claude), "ui");
        assert!(ro.args.contains(&"plan".to_string()), "只读成员该用 plan 模式");
        assert!(
            !ro.args.contains(&"bypassPermissions".to_string()),
            "只读成员不能拿到 bypassPermissions"
        );

        let rw = claude_cmd(&spec(false, BackendKind::Claude), "ui");
        assert!(rw.args.contains(&"bypassPermissions".to_string()), "写成员照旧全权");
        assert!(!rw.args.contains(&"plan".to_string()));
    }

    /// codex 侧要对齐:只读用 --sandbox read-only,不给 bypass。
    #[test]
    fn read_only_member_codex_is_sandboxed() {
        let ro = codex_cmd(&spec(true, BackendKind::Codex), "ui");
        assert!(ro.args.contains(&"read-only".to_string()));
        assert!(!ro.args.iter().any(|a| a.contains("dangerously-bypass")));

        let rw = codex_cmd(&spec(false, BackendKind::Codex), "ui");
        assert!(rw.args.iter().any(|a| a.contains("dangerously-bypass")));
        assert!(!rw.args.contains(&"read-only".to_string()));
    }
}
