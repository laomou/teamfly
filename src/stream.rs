//! backend 流式输出解析。claude 与 codex 的 stream-json 格式不同,各有一个 classifier。
//!
//! 统一产出 `StreamOutcome`(纯数据),backend.rs 拿它更新累积文本/结果/错误并推 UI。

/// stream 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFmt {
    /// claude CLI:{type:system|assistant|result}
    Claude,
    /// codex CLI --json:{type:thread.started|turn.*|item.completed|error}
    Codex,
}

/// 一行 stream 解析结果(纯数据,便于单测)。
#[derive(Debug, Default, PartialEq)]
pub struct StreamOutcome {
    /// 发给 UI 的人类可读行
    pub display: Vec<String>,
    /// 助手文本(累积进 full)
    pub text_delta: Option<String>,
    /// 最终结果文本(整轮汇报)
    pub result: Option<String>,
    /// 错误内容(非空则整轮失败)
    pub error: Option<String>,
    /// 工具执行结果摘要(如 Read 读到的内容, Bash 的输出)
    pub tool_results: Vec<String>,
    /// 思考链文本(extended thinking 中间推理过程)
    pub thinking_tokens: Vec<String>,
}

/// 按格式分类一行。
pub fn classify(fmt: StreamFmt, line: &str) -> StreamOutcome {
    match fmt {
        StreamFmt::Claude => classify_claude(line),
        StreamFmt::Codex => classify_codex(line),
    }
}

/// 解析 JSON 行的公共前处理:空行返回空,非 JSON 原样透出。
fn parse_json(line: &str) -> Result<serde_json::Value, StreamOutcome> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(StreamOutcome::default());
    }
    match serde_json::from_str(trimmed) {
        Ok(v) => Ok(v),
        Err(_) => {
            let mut out = StreamOutcome::default();
            out.display.push(line.to_string());
            Err(out)
        }
    }
}

/// claude CLI 的 stream-json:system(init/thinking_tokens) / assistant(text+tool_use+tool_result) / result。
fn classify_claude(line: &str) -> StreamOutcome {
    let v = match parse_json(line) {
        Ok(v) => v,
        Err(out) => return out,
    };
    let mut out = StreamOutcome::default();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            match v.get("subtype").and_then(|s| s.as_str()) {
                Some("init") => {
                    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
                    let ntools = v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);
                    out.display.push(format!("⟨init⟩ model={model} tools={ntools}"));
                }
                Some("thinking_tokens") => {
                    // 捕获思考链信息
                    if let Some(tokens) = v.get("estimated_tokens").and_then(|t| t.as_u64()) {
                        out.display.push(format!("💭 思考中... ({tokens} tokens)"));
                    }
                    // 如果有 thinking 文本(部分 thinking 模式下会有),捕获为思考链
                    if let Some(text) = v.get("thinking").and_then(|t| t.as_str()) {
                        for line in text.lines() {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                out.thinking_tokens.push(trimmed.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
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
                        Some("tool_result") => {
                            // 工具执行结果:展示摘要
                            if let Some(content) = blk.get("content").and_then(|c| c.as_str()) {
                                let summary = truncate(content, 200);
                                let line = format!("  📋 {summary}");
                                out.display.push(line.clone());
                                out.tool_results.push(summary);
                            } else if let Some(error) = blk.get("is_error").and_then(|e| e.as_bool()) {
                                if error {
                                    let err_msg = blk.get("error").and_then(|e| e.as_str()).unwrap_or("执行失败");
                                    let line = format!("  ❌ {err_msg}");
                                    out.display.push(line);
                                    out.tool_results.push(format!("❌ {err_msg}"));
                                }
                            }
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

/// codex CLI 的 --json JSONL:
///   thread.started / turn.started / turn.completed — 生命周期(基本忽略)
///   item.completed {item:{type, text|message|...}} — 输出项
///   error {message} — 错误
fn classify_codex(line: &str) -> StreamOutcome {
    let v = match parse_json(line) {
        Ok(v) => v,
        Err(out) => return out,
    };
    let mut out = StreamOutcome::default();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("thread.started") => {
            out.display.push("⟨codex 会话开始⟩".to_string());
        }
        Some("turn.started") | Some("turn.completed") => {
            // 生命周期,不展示
        }
        Some("item.completed") => {
            let item = v.get("item");
            let itype = item
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match itype {
                // 助手输出文本项:字段可能叫 text 或 message
                "agent_message" | "assistant_message" | "message" | "text" => {
                    let txt = item
                        .and_then(|i| i.get("text").or_else(|| i.get("message")))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if !txt.is_empty() {
                        for ln in txt.lines() {
                            out.display.push(ln.to_string());
                        }
                        out.text_delta = Some(txt.trim_end().to_string());
                        // codex 没有单独的 result 事件,末条 message 即最终文本
                        out.result = Some(txt.trim().to_string());
                    }
                }
                // 工具/命令执行项
                "command_execution" | "tool_call" | "function_call" => {
                    let hint = item
                        .and_then(|i| i.get("command").or_else(|| i.get("name")))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    out.display.push(format!("🔧 {hint}"));
                }
                // 错误项
                "error" => {
                    let msg = item
                        .and_then(|i| i.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("(未知错误)");
                    out.error = Some(msg.to_string());
                }
                _ => {}
            }
        }
        Some("error") => {
            let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("(未知错误)");
            out.error = Some(msg.to_string());
        }
        _ => {}
    }
    out
}

/// 从 tool_use 的 input 里取一个简短提示(文件路径 / 命令 / query)。
pub fn tool_input_hint(input: Option<&serde_json::Value>) -> String {
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

/// 截断字符串到指定字符数,末尾加 …。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut r: String = s.chars().take(max).collect();
        r.push('…');
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- claude ----

    #[test]
    fn claude_system_init() {
        let line = r#"{"type":"system","subtype":"init","model":"claude-opus","tools":["Read","Bash","Edit"]}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec!["⟨init⟩ model=claude-opus tools=3"]);
        assert!(o.result.is_none() && o.error.is_none());
    }

    #[test]
    fn claude_system_thinking_tokens() {
        let line = r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":8,"thinking":"逐步分析\nauth.py 中\n缺少空指针检查"}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec!["💭 思考中... (8 tokens)"]);
        assert_eq!(o.thinking_tokens, vec!["逐步分析", "auth.py 中", "缺少空指针检查"]);
    }

    #[test]
    fn claude_tool_result_shows_summary() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","content":"def login():\n    pass\n"}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert!(o.display.iter().any(|d| d.contains("📋")));
        assert_eq!(o.tool_results.len(), 1);
    }

    #[test]
    fn claude_assistant_text_and_tool() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"看一下代码"},{"type":"tool_use","name":"Read","input":{"file_path":"auth.py"}}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.text_delta.as_deref(), Some("看一下代码"));
        assert_eq!(o.display, vec!["看一下代码".to_string(), "🔧 Read(auth.py)".to_string()]);
    }

    #[test]
    fn claude_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"干完了 @QE 补测试"}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.result.as_deref(), Some("干完了 @QE 补测试"));
        assert!(o.error.is_none());
    }

    #[test]
    fn claude_result_error() {
        let line = r#"{"type":"result","subtype":"error","is_error":true,"result":"overloaded"}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.error.as_deref(), Some("overloaded"));
        assert!(o.result.is_none());
    }

    #[test]
    fn non_json_passthrough() {
        let o = classify(StreamFmt::Claude, "not json at all");
        assert_eq!(o.display, vec!["not json at all"]);
        let o2 = classify(StreamFmt::Codex, "also not json");
        assert_eq!(o2.display, vec!["also not json"]);
    }

    #[test]
    fn tool_hint_prefers_path() {
        let v = serde_json::json!({"file_path":"src/main.rs","other":"x"});
        assert_eq!(tool_input_hint(Some(&v)), "src/main.rs");
        let v2 = serde_json::json!({"command":"cargo test"});
        assert_eq!(tool_input_hint(Some(&v2)), "cargo test");
    }

    // ---- codex ----

    #[test]
    fn codex_thread_started() {
        let o = classify(StreamFmt::Codex, r#"{"type":"thread.started","thread_id":"x"}"#);
        assert_eq!(o.display, vec!["⟨codex 会话开始⟩"]);
    }

    #[test]
    fn codex_turn_events_silent() {
        assert!(classify(StreamFmt::Codex, r#"{"type":"turn.started"}"#).display.is_empty());
        assert!(classify(StreamFmt::Codex, r#"{"type":"turn.completed"}"#).display.is_empty());
    }

    #[test]
    fn codex_agent_message_is_result() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex 干完了"}}"#;
        let o = classify(StreamFmt::Codex, line);
        assert_eq!(o.result.as_deref(), Some("codex 干完了"));
        assert_eq!(o.display, vec!["codex 干完了"]);
    }

    #[test]
    fn codex_command_execution_tool_line() {
        let line = r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls -la"}}"#;
        let o = classify(StreamFmt::Codex, line);
        assert_eq!(o.display, vec!["🔧 ls -la"]);
    }

    #[test]
    fn codex_error_event() {
        let line = r#"{"type":"error","message":"401 Unauthorized"}"#;
        let o = classify(StreamFmt::Codex, line);
        assert_eq!(o.error.as_deref(), Some("401 Unauthorized"));
    }
}
