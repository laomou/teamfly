//! backend 驱动:无状态每轮重起。喂 (system_prompt, user_input) → 流式产出 raw 行 → 结束汇总。
//! claude/codex 走子进程 headless;api 走 Anthropic 原生 HTTP;mock 供无凭证测试。

use crate::model::{BackendKind, Msg};
use crate::router::strip_ansi;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

pub struct RunSpec {
    pub name: String,
    pub backend: BackendKind,
    pub model: Option<String>,
    #[allow(dead_code)] // provider 已在派发时解析成 base_url/api_key
    pub provider: Option<String>,
    pub system_prompt: String,
    pub user_input: String,
    pub work_dir: PathBuf,
    /// api backend 的 base_url / key(由 provider 解析后传入)
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// 起一个 agent 干一轮活。流式把每行 raw 通过 tx 回投,结束回投 AgentDone。
/// 本函数应在 tokio task 中调用。失败自动重试(应对中转站 429/5xx 等瞬时错误)。
pub async fn run(spec: RunSpec, tx: UnboundedSender<Msg>) {
    let name = spec.name.clone();
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        let result = match spec.backend {
            BackendKind::Claude => run_process(&spec, &tx, claude_cmd(&spec)).await,
            BackendKind::Codex => run_process(&spec, &tx, codex_cmd(&spec)).await,
            BackendKind::Api => run_api(&spec, &tx).await,
            BackendKind::Mock => run_mock(&spec, &tx).await,
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

/// 构造 claude CLI 命令(headless print + bypass 权限 + 追加系统 prompt)。
fn claude_cmd(spec: &RunSpec) -> ProcSpec {
    let mut args = vec![
        "--print".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--append-system-prompt".to_string(),
        spec.system_prompt.clone(),
    ];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    args.push(spec.user_input.clone());
    ProcSpec {
        bin: "claude".into(),
        args,
    }
}

/// 构造 codex CLI 非交互命令。codex exec 走一次性执行。
fn codex_cmd(spec: &RunSpec) -> ProcSpec {
    // codex 无「追加系统 prompt」的稳定 flag,MVP 把系统 prompt 前置进输入。
    let combined = format!("{}\n\n{}", spec.system_prompt, spec.user_input);
    let mut args = vec!["exec".to_string()];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    args.push(combined);
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
) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new(&proc.bin)
        .args(&proc.args)
        .current_dir(&spec.work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("起 `{}` 失败: {e}", proc.bin))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let mut full = String::new();
    let mut err_tail: Vec<String> = Vec::new(); // 保留最后几行 stderr 供报错
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            line = out_reader.next_line() => {
                match line? {
                    Some(l) => {
                        let clean = strip_ansi(&l);
                        full.push_str(&clean);
                        full.push('\n');
                        let _ = tx.send(Msg::AgentStdout { name: spec.name.clone(), line: clean });
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
    Ok(full)
}

/// 只保留最后 8 行 stderr。
fn push_tail(tail: &mut Vec<String>, line: &str) {
    tail.push(line.to_string());
    while tail.len() > 8 {
        tail.remove(0);
    }
}

/// api backend:Anthropic 原生 messages API(非流式,MVP 一次拿回)。
async fn run_api(spec: &RunSpec, tx: &UnboundedSender<Msg>) -> anyhow::Result<String> {
    let base = spec
        .base_url
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let key = spec
        .api_key
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .or_else(|| std::env::var("ANTHROPIC_AUTH_TOKEN").ok())
        .ok_or_else(|| anyhow::anyhow!("api backend 缺 API key"))?;
    let model = spec
        .model
        .clone()
        .unwrap_or_else(|| "claude-sonnet-4-5".to_string());

    let url = format!("{}/v1/messages", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 2048,
        "system": spec.system_prompt,
        "messages": [{"role": "user", "content": spec.user_input}],
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("x-api-key", &key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;

    if !resp.status().is_success() {
        let code = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("API {code}: {}", strip_ansi(&txt));
    }
    let v: serde_json::Value = resp.json().await?;
    let text = v["content"]
        .as_array()
        .and_then(|arr| {
            let mut s = String::new();
            for blk in arr {
                if let Some(t) = blk["text"].as_str() {
                    s.push_str(t);
                }
            }
            Some(s)
        })
        .unwrap_or_default();

    for line in text.lines() {
        let _ = tx.send(Msg::AgentStdout {
            name: spec.name.clone(),
            line: line.to_string(),
        });
    }
    Ok(text)
}

/// mock backend:确定性产出,无需凭证。用于端到端/CI。
/// 老K 会拆活并 @阿码;其余人回一句完成。
async fn run_mock(spec: &RunSpec, tx: &UnboundedSender<Msg>) -> anyhow::Result<String> {
    let steps: Vec<String> = if spec.name.contains('K') || spec.system_prompt.contains("架构") {
        vec![
            format!("[{}] 收到目标,分析中…", spec.name),
            "读了一下项目结构".to_string(),
            "【群聊】拆成两块:实现与测试。@阿码 你接实现,完成后 @阿测 补测试".to_string(),
        ]
    } else if spec.name.contains("测") {
        vec![
            format!("[{}] 开始补测试", spec.name),
            "【群聊】测试写完,全绿 ✓".to_string(),
        ]
    } else {
        vec![
            format!("[{}] 开始干活", spec.name),
            "改了几个文件".to_string(),
            format!("【群聊】{} 干完了 (+12 -3)", spec.name),
        ]
    };
    for s in &steps {
        // 轻微延时,模拟流式;用 tokio sleep(不依赖 Instant::now 之类被禁 API)
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = tx.send(Msg::AgentStdout {
            name: spec.name.clone(),
            line: s.clone(),
        });
    }
    Ok(steps.join("\n"))
}
