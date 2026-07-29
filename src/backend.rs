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
    pub backend: BackendKind,
    pub model: Option<String>,
    /// 注入到子进程的环境变量(来自 .teamfly/env.toml,全队共享)
    pub env: std::collections::HashMap<String, String>,
    pub system_prompt: String,
    pub user_input: String,
    pub work_dir: PathBuf,
    /// 预解析好的 MCP 配置文件路径(查的是主 work_dir,不是 worktree)
    pub mcp_config: Option<String>,
    /// 这一轮的 worktree (目录, 分支);fallback 时为 None。原样回投给 AgentDone。
    pub worktree: Option<(PathBuf, String)>,
    /// 只读:不给写权限。用于 `worktree: false` 的成员 ——
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
        "AskUserQuestion".to_string(), // 无拍板:禁 agent 反问
        "--append-system-prompt".to_string(),
        spec.system_prompt.clone(),
    ];
    // 模型不走 --model,改由 env ANTHROPIC_MODEL 接管(见 run_process 里的注入逻辑)
    // MCP 配置已在 spawn 时从主 work_dir 解析好(worktree 里没有 .teamfly/)
    if let Some(mcp) = &spec.mcp_config {
        args.push("--mcp-config".into());
        args.push(mcp.clone());
        args.push("--strict-mcp-config".into());
    }
    args.push(user_input.to_string());
    ProcSpec {
        bin: "claude".into(),
        args,
    }
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
    // codex 无「追加系统 prompt」的稳定 flag,把系统 prompt 前置进输入。
    let combined = format!("{}\n\n{}", spec.system_prompt, user_input);
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),                // JSONL 事件流
        "--skip-git-repo-check".to_string(), // 不要求工作目录是 git 库
    ];
    if spec.read_only {
        // 只读成员直接在用户主工作树里跑,必须禁写(和 claude 的 plan 模式对齐)
        args.push("--sandbox".to_string());
        args.push("read-only".to_string());
    } else {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string()); // 无拍板
    }
    args.push(combined);
    // 模型不走 --model,改由 env 接管
    ProcSpec {
        bin: "codex".into(),
        args,
    }
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

    // 模型不走 --model,改由 env 注入
    // 优先级:继承值 < env.toml(spec.env) < frontmatter model(最高,cmd.env 最后覆盖)
    // 先注入 env.toml
    cmd.envs(&spec.env);
    // 再覆盖 frontmatter model(若指定)
    if let Some(m) = &spec.model {
        let env_key = match spec.backend {
            BackendKind::Claude => "ANTHROPIC_MODEL",
            BackendKind::Codex => "OPENAI_MODEL",
        };
        cmd.env(env_key, m);
    }

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
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            biased; // 优先响应取消
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
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

    fn spec(read_only: bool, backend: BackendKind) -> RunSpec {
        RunSpec {
            name: "X".into(),
            issue: 1,
            gen: 0,
            backend,
            model: None,
            env: std::collections::HashMap::new(),
            system_prompt: "sp".into(),
            user_input: "ui".into(),
            work_dir: PathBuf::from("/tmp"),
            mcp_config: None,
            worktree: None,
            read_only,
        }
    }

    /// `worktree: false` 的成员直接在**用户主工作树**里跑,必须只读。
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
