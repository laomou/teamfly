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
    /// 见到了 thinking 块但**内容是空的**(某些中转站会把正文剥掉只留 signature)。
    ///
    /// 由调用方决定要不要提示 —— classify 是按行调用的纯函数,没有跨行状态,
    /// 在这里直接 push 提示的话一轮里有几个 thinking 块就会刷几条。
    pub empty_thinking: bool,
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
                            let t = blk.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            let mut any = false;
                            for ln in t.lines() {
                                let ln = ln.trim_end();
                                if !ln.is_empty() {
                                    out.display.push(format!("💭 {ln}"));
                                    any = true;
                                }
                            }
                            // 有 thinking 块但**内容是空的** —— 实测某些中转站会把
                            // 正文剥掉只留 signature。这里只置标志,提不提示交给
                            // 调用方(它有整轮状态,能做到一轮只说一次)。
                            if !any {
                                out.empty_thinking = true;
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
                    let rows = result_lines(&body, TOOL_RESULT_MAX_LINES, TOOL_RESULT_MAX_COLS);
                    let is_err = blk
                        .get("is_error")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false);
                    if is_err {
                        // 失败时错误文本也在 content 里,没有单独的 error 字段
                        if rows.is_empty() {
                            out.display.push("❌ 执行失败".to_string());
                        } else {
                            // 首行带图标,续行用空格对齐 —— tui 那边按前缀分类,
                            // 续行不能再带 ❌ 否则每行都被当成一个新错误
                            for (i, r) in rows.iter().enumerate() {
                                out.display
                                    .push(if i == 0 { format!("❌ {r}") } else { format!("   {r}") });
                            }
                        }
                    } else {
                        for (i, r) in rows.iter().enumerate() {
                            out.display
                                .push(if i == 0 { format!("📋 {r}") } else { format!("   {r}") });
                        }
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

/// tool_result 摘要保留的最大字符数。
/// 工具结果最多显示多少行。结果可能上千行,全塞进 raw 缓冲会把别的轮次挤掉。
const TOOL_RESULT_MAX_LINES: usize = 20;
/// 工具结果单行最多多少字符(一行几千字符会把 raw 视图撑爆)。
const TOOL_RESULT_MAX_COLS: usize = 200;

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
/// 把工具结果切成**保留行结构**的若干行,供 raw 视图逐行加缩进。
///
/// 以前走 `summarize`,它用 `split_whitespace()` 把整段压成一行 ——
/// Read 一个文件、跑一次测试返回几十行,全糊成一行再砍到 200 字,
/// 结构和内容一起没了。
///
/// 行数上限:结果可能上千行,全塞进 raw 缓冲会把别的轮次挤掉(RAW_CAP)。
/// 超出时保**前面**几行(文件头/报错开头信息量最大)并说明省了多少。
fn result_lines(s: &str, max_lines: usize, max_cols: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let total = s.lines().count();
    for l in s.lines().take(max_lines) {
        // 制表符换成空格:终端里 tab 跳到 8 列对齐,和 tui 那边加的缩进前缀
        // 叠在一起会错位(`cat -n` / Read 的行号就是 tab 分隔的)
        let l = l.replace('\t', "    ");
        let l = l.trim_end();
        // 单行超长仍要截 —— 一行几千字符会把 raw 视图撑爆
        if l.chars().count() > max_cols {
            let mut t: String = l.chars().take(max_cols).collect();
            t.push('…');
            out.push(t);
        } else {
            out.push(l.to_string());
        }
    }
    if total > max_lines {
        out.push(format!("…(还有 {} 行)", total - max_lines));
    }
    // 全是空行时别推一堆空的
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
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

    /// thinking 事件内容完整时,该逐行出 💭。
    #[test]
    fn real_thinking_becomes_display_lines() {
        let line = include_str!("../tests/fixtures/thinking_full.json");
        let o = classify(StreamFmt::Claude, line.trim());
        assert!(
            o.display.iter().filter(|d| d.starts_with("💭")).count() >= 3,
            "该有多行思考,实际 {:?}",
            o.display
        );
        assert!(!o.empty_thinking, "内容是全的,不该标记为空");
        // 思考不能混进汇报正文 —— 它不是给队友看的
        assert!(o.text_delta.is_none(), "思考混进了 text_delta");
    }

    /// **块在但 thinking 字段是空串** —— 实测有的中转站会这样,
    /// signature 留着,正文被剥掉。
    ///
    /// 以前这种情况完全静默,用户分不清是模型没思考、中转站剥了、还是
    /// teamfly 坏了,只看到「从来没有 💭」。
    #[test]
    fn empty_thinking_is_flagged_not_silent() {
        let line = include_str!("../tests/fixtures/thinking_empty.json");
        let o = classify(StreamFmt::Claude, line.trim());
        assert!(o.display.is_empty(), "空内容不该产出 💭 行");
        assert!(o.empty_thinking, "该标记出来,否则调用方没法提示用户");
    }

    /// 没有 thinking 块时不能误报(绝大多数事件都是这种)。
    #[test]
    fn no_thinking_block_does_not_flag() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"你好"}]}}"#;
        let o = classify(StreamFmt::Claude, line);
        assert!(!o.empty_thinking, "没有 thinking 块却报了空");
    }


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
        // 保留行结构:首行带 📋,续行用三空格对齐(tui 那边靠这个前缀认出是续行)。
        // 制表符展开成空格,不然和 tui 加的缩进叠在一起会错位。
        assert_eq!(
            o.display,
            vec![
                r#"📋 1    [package]"#,
                r#"   2    name = "teamfly""#,
            ]
        );
    }

    /// `result_lines` 本身保留行结构。
    #[test]
    fn tool_result_keeps_line_structure() {
        let body = "line one\nline two\nline three";
        let rows = result_lines(body, 20, 200);
        assert_eq!(rows, vec!["line one", "line two", "line three"]);
    }

    /// 走**完整 classify 路径**确认多行结果没被压平。
    ///
    /// 只测 `result_lines` 不够 —— 那只守函数本身,不守调用点有没有接上。
    /// (实测过:把调用点换回 split_whitespace 压平,只有这条会红。)
    #[test]
    fn classify_does_not_flatten_tool_result() {
        let line = "{\"type\":\"user\",\"message\":{\"content\":[{\"tool_use_id\":\"t1\",\
\"type\":\"tool_result\",\"content\":\"A one\\nB two\\nC three\"}]}}";
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display.len(), 3, "三行结果被压成了 {} 行", o.display.len());
        assert!(o.display[0].starts_with("📋 A one"));
        assert!(o.display[1].starts_with("   B two"));
        assert!(o.display[2].starts_with("   C three"));
    }

    /// 行数超上限时保**前面**几行(文件头/报错开头信息量最大),并说明省了多少。
    #[test]
    fn tool_result_caps_lines_and_says_how_many_dropped() {
        let body: String = (1..=50).map(|i| format!("L{i}\n")).collect();
        let rows = result_lines(&body, 20, 200);
        assert_eq!(rows.len(), 21, "20 行 + 1 行说明");
        assert_eq!(rows[0], "L1");
        assert_eq!(rows[19], "L20");
        assert!(rows[20].contains("还有 30 行"), "没说省了多少: {:?}", rows[20]);
    }

    /// 单行超长要截,否则一行几千字符会把 raw 视图撑爆。
    #[test]
    fn tool_result_truncates_huge_single_line() {
        let body = "x".repeat(500);
        let rows = result_lines(&body, 20, 200);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chars().count(), 201, "200 字符 + 省略号");
        assert!(rows[0].ends_with('…'));
    }

    /// 多行**错误**结果同样保留结构,且续行不能再带 ❌ ——
    /// tui 按前缀分类,每行都带 ❌ 会被当成一串独立错误。
    #[test]
    fn multiline_error_keeps_icon_only_on_first_line() {
        let line = "{\"type\":\"user\",\"message\":{\"content\":[{\"tool_use_id\":\"t1\",\
\"type\":\"tool_result\",\"is_error\":true,\
\"content\":\"error: 第一行\\nerror: 第二行\"}]}}";
        let o = classify(StreamFmt::Claude, line);
        assert_eq!(o.display.len(), 2);
        assert!(o.display[0].starts_with("❌ "));
        assert!(
            o.display[1].starts_with("   ") && !o.display[1].contains('❌'),
            "续行不该再带 ❌: {:?}",
            o.display[1]
        );
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
