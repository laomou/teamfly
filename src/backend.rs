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
    pub backend: BackendKind,
    pub model: Option<String>,
    /// 注入到子进程的环境变量(来自 .teamfly/env.toml,全队共享)
    pub env: std::collections::HashMap<String, String>,
    pub system_prompt: String,
    pub user_input: String,
    pub work_dir: PathBuf,
}

/// 起一个 agent 干一轮活。流式把每行 raw 通过 tx 回投,结束回投 AgentDone。
/// 本函数应在 tokio task 中调用。失败自动重试(应对中转站 429/5xx 等瞬时错误)。
/// `cancel` 用于外部取消(如用户按 Ctrl+C)。
pub async fn run(spec: RunSpec, cancel: CancellationToken, tx: UnboundedSender<Msg>) {
    let name = spec.name.clone();
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        // 检查是否被取消
        if cancel.is_cancelled() {
            let _ = tx.send(Msg::AgentDone {
                name,
                full_output: String::new(),
                ok: false,
                err: Some("用户取消".into()),
            });
            return;
        }
        let result = match spec.backend {
            BackendKind::Claude => run_process(&spec, &tx, claude_cmd(&spec), crate::stream::StreamFmt::Claude, &cancel).await,
            BackendKind::Codex => run_process(&spec, &tx, codex_cmd(&spec), crate::stream::StreamFmt::Codex, &cancel).await,
        };

        match result {
            Ok(full) => {
                let _ = tx.send(Msg::AgentDone {
                    name,
                    full_output: full,
                    ok: true,
                    err: None,
                });
                return;
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt < MAX_ATTEMPTS {
                    // 提示这次失败要重试,并退避
                    let _ = tx.send(Msg::AgentStdout {
                        name: name.clone(),
                        line: format!("⟨err⟩ 第{attempt}次失败,重试中… {last_err}"),
                    });
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

    // 重试用尽,报掉线
    let _ = tx.send(Msg::AgentDone {
        name,
        full_output: String::new(),
        ok: false,
        err: Some(format!("重试 {MAX_ATTEMPTS} 次仍失败:{last_err}")),
    });
}

/// 构造 claude CLI 命令(headless stream-json + bypass 权限 + 禁反问 + 追加系统 prompt)。
fn claude_cmd(spec: &RunSpec) -> ProcSpec {
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(), // stream-json 必需
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--disallowedTools".to_string(),
        "AskUserQuestion".to_string(), // 无拍板:禁 agent 反问
        "--append-system-prompt".to_string(),
        spec.system_prompt.clone(),
    ];
    // 模型不走 --model,改由 env ANTHROPIC_MODEL 接管(见 run_process 里的注入逻辑)
    // MCP 两级 fallback:项目级 <work_dir>/.teamfly/mcp.json > 用户级 ~/.teamfly/mcp.json
    if let Some(mcp) = resolve_mcp_config(&spec.work_dir) {
        args.push("--mcp-config".into());
        args.push(mcp);
        args.push("--strict-mcp-config".into());
    }
    args.push(spec.user_input.clone());
    ProcSpec {
        bin: "claude".into(),
        args,
    }
}

/// MCP 配置文件:项目级优先,回退到用户级。都不存在返回 None。
fn resolve_mcp_config(work_dir: &std::path::Path) -> Option<String> {
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
fn codex_cmd(spec: &RunSpec) -> ProcSpec {
    // codex 无「追加系统 prompt」的稳定 flag,把系统 prompt 前置进输入。
    let combined = format!("{}\n\n{}", spec.system_prompt, spec.user_input);
    let args = vec![
        "exec".to_string(),
        "--json".to_string(),                              // JSONL 事件流
        "--skip-git-repo-check".to_string(),               // 不要求工作目录是 git 库
        "--dangerously-bypass-approvals-and-sandbox".to_string(), // 无拍板,和 claude 对齐
        combined,
    ];
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
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(&proc.bin);
    cmd.args(&proc.args)
        .current_dir(&spec.work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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

    let mut full = String::new();
    let mut result_text: Option<String> = None;
    let mut stream_err: Option<String> = None;
    let mut err_tail: Vec<String> = Vec::new();
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    // 先发一轮 init 标记,让 raw 视图能识别轮次(无论 claude/codex 都有效)
    let _ = tx.send(Msg::AgentStdout {
        name: spec.name.clone(),
        line: "⟨init⟩ 新一轮".into(),
    });

    loop {
        tokio::select! {
            biased; // 优先检查取消
            _ = cancel.cancelled() => {
                // 取消:kill 子进程
                let _ = child.kill().await;
                anyhow::bail!("用户取消");
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
                        if let Some(e) = outcome.error {
                            stream_err = Some(e);
                        } else if let Some(r) = outcome.result {
                            result_text = Some(r);
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
        // 检查是否被取消(在 select 之后)
        if cancel.is_cancelled() {
            let _ = child.kill().await;
            anyhow::bail!("用户取消");
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

    // stream-json 里 result.is_error 视为失败(即便退出码 0)
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

