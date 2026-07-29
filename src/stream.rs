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
    /// **致命**错误内容(非空则整轮失败)
    pub error: Option<String>,
    /// **瞬时/可恢复**错误(如 codex 的 "Reconnecting... 2/5")。
    /// 只展示,不判整轮失败 —— 重连成功后这一轮照样能出结果。
    /// 仅在整轮既无 result 又无正文时被当作兜底失败原因。
    pub warn: Option<String>,
    /// 这一行是否表示 agent 动过工具(读写文件、跑命令)。
    /// 重试前要看它:动过工具说明可能已经改了工作区,不能无脑把同一个 prompt 从头再跑一遍。
    pub tool_used: bool,
}

/// 按格式分类一行。
pub fn classify(fmt: StreamFmt, line: &str) -> StreamOutcome {
    match fmt {
        StreamFmt::Claude => classify_claude(line),
        StreamFmt::Codex => classify_codex(line),
    }
}

/// `parse_json` 的结果。`Done` 不是错误路径 —— 它装的是**已经算好的结果**
/// (空行 → 空;非 JSON → 原样透出),调用方直接 return 就行。
enum Parsed {
    Json(serde_json::Value),
    Done(StreamOutcome),
}

/// 解析 JSON 行的公共前处理:空行返回空,非 JSON 原样透出。
fn parse_json(line: &str) -> Parsed {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Done(StreamOutcome::default());
    }
    match serde_json::from_str(trimmed) {
        Ok(v) => Parsed::Json(v),
        Err(_) => {
            let mut out = StreamOutcome::default();
            out.display.push(line.to_string());
            Parsed::Done(out)
        }
    }
}

/// claude CLI 的 stream-json:
///   system(init)                        — 会话头
///   assistant(text / thinking / tool_use)
///   user(tool_result)                   — 工具执行结果是用一条 user 消息回投的,不在 assistant 里
///   result                              — 整轮结束
fn classify_claude(line: &str) -> StreamOutcome {
    let v = match parse_json(line) {
        Parsed::Json(v) => v,
        Parsed::Done(out) => return out,
    };
    let mut out = StreamOutcome::default();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => {
            // 只显示 init。thinking_tokens 是每几十毫秒一发的累计计数器(一轮几十条,
            // 且只有 estimated_tokens 没有文本),显示就是刷屏;思考文本走 assistant/thinking。
            if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
                let ntools = v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);
                out.display.push(format!("⟨init⟩ model={model} tools={ntools}"));
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
                            if !is_readonly_tool(name) {
                                out.tool_used = true;
                            }
                        }
                        // extended thinking 的中间推理:只展示,不进 text_delta
                        //(它不是给队友看的回复正文,混进 full 会污染汇报)
                        Some("thinking") => {
                            if let Some(t) = blk.get("thinking").and_then(|t| t.as_str()) {
                                for ln in t.lines() {
                                    let ln = ln.trim_end();
                                    if !ln.is_empty() {
                                        out.display.push(format!("💭 {ln}"));
                                    }
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
        Some("user") => {
            // 工具执行结果。user 消息里的 text 块是喂进去的 prompt,不回显。
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for blk in content {
                    if blk.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let body = tool_result_text(blk.get("content"));
                    let summary = summarize(&body, TOOL_RESULT_MAX);
                    if blk.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
                        // 失败时错误文本也在 content 里,没有单独的 error 字段
                        let msg = if summary.is_empty() { "执行失败".to_string() } else { summary };
                        out.display.push(format!("❌ {msg}"));
                    } else if !summary.is_empty() {
                        out.display.push(format!("📋 {summary}"));
                    }
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
///   turn.failed {error:{message}}                  — 整轮真失败
///   item.completed {item:{type, text|message|...}} — 输出项(含 item.type=error)
///   error {message}                                — **瞬时**错误,多为 "Reconnecting... 2/5",
///                                                    重连成功后照常出结果,不能当整轮失败
fn classify_codex(line: &str) -> StreamOutcome {
    let v = match parse_json(line) {
        Parsed::Json(v) => v,
        Parsed::Done(out) => return out,
    };
    let mut out = StreamOutcome::default();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("thread.started") => {
            out.display.push("⟨codex 会话开始⟩".to_string());
        }
        Some("turn.started") | Some("turn.completed") => {
            // 生命周期,不展示
        }
        // 整轮失败的正式收尾事件(实测存在,字段是 error.message)
        Some("turn.failed") => {
            let msg = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("(未知错误)");
            out.error = Some(msg.to_string());
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
                    out.tool_used = true;
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
            // 瞬时错误:codex 网络抖动时会连发 "Reconnecting... 2/5 (...)",
            // 重连成功后照常给出完整答案并以 turn.completed 收尾、退出码 0。
            // 当成整轮失败会把已经拿到的结果丢掉并从零重跑,所以只展示不判失败。
            let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("(未知错误)");
            out.display.push(format!("⚠ {msg}"));
            out.warn = Some(msg.to_string());
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

/// tool_result 摘要保留的最大字符数。
const TOOL_RESULT_MAX: usize = 200;

/// tool_result 的 content:可能是字符串,也可能是内容块数组(取里面的 text 块,图片等跳过)。
fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => {
            let mut buf = String::new();
            for b in blocks {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if !buf.is_empty() {
                        buf.push(' ');
                    }
                    buf.push_str(t);
                }
            }
            buf
        }
        _ => String::new(),
    }
}

/// 压成单行(空白折叠)再截断到 max 字符,超长补 …。
/// 折成单行是必须的:一条 display 就是 raw 视图里的一行,内嵌换行会把渲染搞乱。
fn summarize(s: &str, max: usize) -> String {
    let mut one = String::new();
    for w in s.split_whitespace() {
        if !one.is_empty() {
            one.push(' ');
        }
        one.push_str(w);
    }
    if one.chars().count() <= max {
        return one;
    }
    let mut r: String = one.chars().take(max).collect();
    r.push('…');
    r
}

/// 只读工具:不改工作区,重试时无需带「先核对现状」的提醒。
/// 宁可漏判(把只读的当成写了 → 多提醒一句,无害),不可误判(把写工具当成只读 → 重试重复副作用)。
fn is_readonly_tool(name: &str) -> bool {
    matches!(
        name,
        "Read" | "Grep" | "Glob" | "WebFetch" | "WebSearch"
            | "TaskList" | "TaskGet" | "CronList"
            | "ListFiles" | "SearchFiles" | "ReadFile" | "GetFile"
    )
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
    fn claude_system_thinking_tokens_ignored() {
        // 真机形状:纯计数器,一轮发几十条,没有文本 —— 显示就是刷屏
        let line = r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":8,"estimated_tokens_delta":3}"#;
        let o = classify(StreamFmt::Claude, line);
        assert!(o.display.is_empty());
    }

    #[test]
    fn claude_assistant_thinking_block_shown_but_not_in_delta() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"先看 auth.py\n\n再补空指针检查"}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec!["💭 先看 auth.py", "💭 再补空指针检查"]);
        // 思考不是回复正文,不能进 full
        assert!(o.text_delta.is_none());
    }

    #[test]
    fn claude_tool_result_comes_from_user_message() {
        // 真机形状:tool_result 挂在 type=user 上,不在 assistant 里
        let line = r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"1\t[package]\n2\tname = \"teamfly\"\n"}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec![r#"📋 1 [package] 2 name = "teamfly""#]);
    }

    #[test]
    fn claude_tool_result_content_array() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"3 passed"},{"type":"image"}]}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec!["📋 3 passed"]);
    }

    #[test]
    fn claude_tool_result_error() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"File does not exist.","is_error":true}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display, vec!["❌ File does not exist."]);
    }

    #[test]
    fn claude_user_prompt_text_not_echoed() {
        let line = r#"{"type":"user","message":{"content":[{"type":"text","text":"帮我改 auth.py"}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert!(o.display.is_empty());
    }

    #[test]
    fn summarize_flattens_and_truncates() {
        // 摘要必须是单行:内嵌换行会把 raw 视图渲染搞乱
        assert_eq!(summarize("a\nb  c\t\nd", 100), "a b c d");
        let long = "x".repeat(250);
        let s = summarize(&long, TOOL_RESULT_MAX);
        assert_eq!(s.chars().count(), TOOL_RESULT_MAX + 1);
        assert!(s.ends_with('…') && !s.contains('\n'));
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
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"干完了 @TPM 请安排评审"}"#;
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.result.as_deref(), Some("干完了 @TPM 请安排评审"));
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
    fn codex_transient_error_is_warn_not_failure() {
        // 真机形状:网络抖动时连发 Reconnecting,重连成功后照常出结果。
        // 当成整轮失败会把已拿到的结果丢掉并从零重跑,所以只能是 warn。
        let line = r#"{"type":"error","message":"Reconnecting... 2/5 (unexpected status 401)"}"#;
        let o = classify(StreamFmt::Codex, line);
        assert!(o.error.is_none());
        assert_eq!(o.warn.as_deref(), Some("Reconnecting... 2/5 (unexpected status 401)"));
        assert_eq!(o.display.len(), 1);
        assert!(o.display[0].starts_with("⚠"));
    }

    #[test]
    fn codex_turn_failed_is_real_failure() {
        // turn.failed 才是整轮失败的正式收尾(实测存在,字段是 error.message)
        let line = r#"{"type":"turn.failed","error":{"message":"context window exceeded"}}"#;
        let o = classify(StreamFmt::Codex, line);
        assert_eq!(o.error.as_deref(), Some("context window exceeded"));
    }

    #[test]
    fn codex_item_error_is_real_failure() {
        // item.completed 里的 error 项是真实存在的(抓包见过),不是死代码
        let line = r#"{"type":"item.completed","item":{"id":"i1","type":"error","message":"stream disconnected"}}"#;
        let o = classify(StreamFmt::Codex, line);
        assert_eq!(o.error.as_deref(), Some("stream disconnected"));
    }
}
