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
    /// per-agent MCP 配置文件路径(仅 claude backend 使用)
    pub mcp_config: Option<String>,
    /// 注入到子进程的环境变量(来自 .teamfly/env.toml,全队共享)
    pub env: std::collections::HashMap<String, String>,
    pub system_prompt: String,
    pub user_input: String,
    pub work_dir: PathBuf,
}

/// 起一个 agent 干一轮活。流式把每行 raw 通过 tx 回投,结束回投 AgentDone。
/// 本函数应在 tokio task 中调用。失败自动重试(应对中转站 429/5xx 等瞬时错误)。
pub async fn run(spec: RunSpec, tx: UnboundedSender<Msg>) {
    let name = spec.name.clone();
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        let result = match spec.backend {
            BackendKind::Claude => run_process(&spec, &tx, claude_cmd(&spec), true).await,
            BackendKind::Codex => run_process(&spec, &tx, codex_cmd(&spec), false).await,
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
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    if let Some(mcp) = &spec.mcp_config {
        args.push("--mcp-config".into());
        args.push(mcp.clone());
        args.push("--strict-mcp-config".into());
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
    stream_json: bool,
) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new(&proc.bin)
        .args(&proc.args)
        .current_dir(&spec.work_dir)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("起 `{}` 失败: {e}", proc.bin))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let mut full = String::new();       // 纯文本 backend:全部输出;stream-json:累积 assistant 文本
    let mut result_text: Option<String> = None; // stream-json 的 result 事件最终文本
    let mut stream_err: Option<String> = None;  // stream-json result.is_error 的内容
    let mut err_tail: Vec<String> = Vec::new(); // 保留最后几行 stderr 供报错
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            line = out_reader.next_line() => {
                match line? {
                    Some(l) => {
                        let clean = strip_ansi(&l);
                        if stream_json {
                            handle_stream_line(&clean, spec, tx, &mut full, &mut result_text, &mut stream_err);
                        } else {
                            full.push_str(&clean);
                            full.push('\n');
                            let _ = tx.send(Msg::AgentStdout { name: spec.name.clone(), line: clean });
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

/// 解析一行 stream-json 事件:更新累积文本 / result / error,并把人类可读的活动行发给 UI。
fn handle_stream_line(
    line: &str,
    spec: &RunSpec,
    tx: &UnboundedSender<Msg>,
    full: &mut String,
    result_text: &mut Option<String>,
    stream_err: &mut Option<String>,
) {
    let outcome = classify_stream_line(line);
    for disp in outcome.display {
        let _ = tx.send(Msg::AgentStdout {
            name: spec.name.clone(),
            line: disp,
        });
    }
    if let Some(t) = outcome.text_delta {
        full.push_str(&t);
        full.push('\n');
    }
    if let Some(e) = outcome.error {
        *stream_err = Some(e);
    } else if let Some(r) = outcome.result {
        *result_text = Some(r);
    }
}

/// 一行 stream-json 解析结果(纯数据,便于单测)。
#[derive(Debug, Default, PartialEq)]
struct StreamOutcome {
    display: Vec<String>,       // 发给 UI 的人类可读行
    text_delta: Option<String>, // assistant 文本(累积进 full)
    result: Option<String>,     // result 事件最终文本
    error: Option<String>,      // result.is_error 内容
}

/// 纯函数:把一行 stream-json 分类成 StreamOutcome。非 JSON 行原样透出。
fn classify_stream_line(line: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return out;
    }
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            out.display.push(line.to_string());
            return out;
        }
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
            let ntools = v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);
            out.display.push(format!("⟨init⟩ model={model} tools={ntools}"));
        }
        Some("assistant") => {
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                let mut delta = String::new();
                for blk in content {
                    match blk.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = blk.get("text").and_then(|t| t.as_str()) {
                                delta.push_str(t);
                                delta.push('\n');
                                for ln in t.lines() {
                                    out.display.push(ln.to_string());
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = blk.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                            let hint = tool_input_hint(blk.get("input"));
                            out.display.push(format!("🔧 {name}({hint})"));
                        }
                        _ => {}
                    }
                }
                if !delta.is_empty() {
                    out.text_delta = Some(delta.trim_end().to_string());
                }
            }
        }
        Some("result") => {
            let is_err = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
            if is_err {
                out.error = Some(if text.is_empty() { "(无 result 文本)".into() } else { text });
            } else if !text.is_empty() {
                out.result = Some(text);
            }
        }
        _ => {}
    }
    out
}

/// 从 tool_use 的 input 里取一个简短提示(文件路径 / 命令 / query)。
fn tool_input_hint(input: Option<&serde_json::Value>) -> String {
    let Some(obj) = input.and_then(|i| i.as_object()) else {
        return String::new();
    };
    for key in ["file_path", "path", "command", "pattern", "query", "url"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            return if s.chars().count() > 60 {
                format!("{}…", s.chars().take(60).collect::<String>())
            } else {
                s.to_string()
            };
        }
    }
    String::new()
}

/// 只保留最后 8 行 stderr。
fn push_tail(tail: &mut Vec<String>, line: &str) {
    tail.push(line.to_string());
    while tail.len() > 8 {
        tail.remove(0);
    }
}

/// api backend:Anthropic 原生 messages API(非流式,MVP 一次拿回)。
/// base_url / key 优先看 spec.env(.teamfly/env.toml),再看进程环境变量。
async fn run_api(spec: &RunSpec, tx: &UnboundedSender<Msg>) -> anyhow::Result<String> {
    let lookup = |k: &str| -> Option<String> {
        spec.env.get(k).cloned().or_else(|| std::env::var(k).ok())
    };
    let base = lookup("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let key = lookup("ANTHROPIC_API_KEY")
        .or_else(|| lookup("ANTHROPIC_AUTH_TOKEN"))
        .ok_or_else(|| anyhow::anyhow!("api backend 缺 API key(在 .teamfly/env.toml 或环境变量里设 ANTHROPIC_API_KEY/ANTHROPIC_AUTH_TOKEN)"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_system_init() {
        let line = r#"{"type":"system","subtype":"init","model":"claude-opus","tools":["Read","Bash","Edit"]}"#;
        let o = classify_stream_line(line);
        assert_eq!(o.display, vec!["⟨init⟩ model=claude-opus tools=3"]);
        assert!(o.result.is_none() && o.error.is_none());
    }

    #[test]
    fn stream_assistant_text_and_tool() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"看一下代码"},{"type":"tool_use","name":"Read","input":{"file_path":"auth.py"}}]}}"#;
        let o = classify_stream_line(line);
        assert_eq!(o.text_delta.as_deref(), Some("看一下代码"));
        assert_eq!(o.display, vec!["看一下代码".to_string(), "🔧 Read(auth.py)".to_string()]);
    }

    #[test]
    fn stream_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"【群聊】干完了 @QE 补测试"}"#;
        let o = classify_stream_line(line);
        assert_eq!(o.result.as_deref(), Some("【群聊】干完了 @QE 补测试"));
        assert!(o.error.is_none());
    }

    #[test]
    fn stream_result_error() {
        let line = r#"{"type":"result","subtype":"error","is_error":true,"result":"overloaded"}"#;
        let o = classify_stream_line(line);
        assert_eq!(o.error.as_deref(), Some("overloaded"));
        assert!(o.result.is_none());
    }

    #[test]
    fn stream_non_json_passthrough() {
        let o = classify_stream_line("not json at all");
        assert_eq!(o.display, vec!["not json at all"]);
    }

    #[test]
    fn tool_hint_prefers_path() {
        let v = serde_json::json!({"file_path":"src/main.rs","other":"x"});
        assert_eq!(tool_input_hint(Some(&v)), "src/main.rs");
        let v2 = serde_json::json!({"command":"cargo test"});
        assert_eq!(tool_input_hint(Some(&v2)), "cargo test");
    }
}
