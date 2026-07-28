//! env.toml —— agent 环境变量,注入到 agent 子进程。
//!
//! 两级加载(后覆盖前):
//!   1. 用户级 `~/.teamfly/env.toml` —— API key 等全局默认放这里,一次配好多项目共用
//!   2. 项目级 `<工作目录>/.teamfly/env.toml` —— 覆盖用户级同名 key,项目特定的东西放这里
//!
//! 格式:
//! ```toml
//! # 顶层 = 全部 backend 共享
//! LOG_LEVEL = "info"
//!
//! [claude]                     # 只注入 claude backend
//! ANTHROPIC_BASE_URL = "https://中转站A.example.com"
//! ANTHROPIC_API_KEY  = "${MY_CLAUDE_KEY}"
//!
//! [codex]                      # 只注入 codex backend
//! OPENAI_API_KEY = "${MY_CODEX_KEY}"
//! ```
//!
//! 合并:全局 + 该 backend 段。同名 key 以 backend 段为准。
//! 值支持 `$VAR` / `${VAR}` 引用当前进程环境变量。

use crate::model::BackendKind;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// backend 段名。
const SECTION_KEYS: &[&str] = &["claude", "codex"];

#[derive(Debug, Default, Clone)]
pub struct AgentEnv {
    global: HashMap<String, String>,
    claude: HashMap<String, String>,
    codex: HashMap<String, String>,
}

impl AgentEnv {
    /// 返回给某 backend 的最终环境变量(全局 + 该段覆盖)。
    pub fn merged_for(&self, kind: BackendKind) -> HashMap<String, String> {
        let mut out = self.global.clone();
        let section = match kind {
            BackendKind::Claude => &self.claude,
            BackendKind::Codex => &self.codex,
        };
        for (k, v) in section {
            out.insert(k.clone(), v.clone());
        }
        out
    }

    #[cfg(test)]
    pub fn from_maps(
        global: HashMap<String, String>,
        claude: HashMap<String, String>,
        codex: HashMap<String, String>,
    ) -> Self {
        AgentEnv { global, claude, codex }
    }
}

/// 加载 env.toml。合并顺序:
///   1. 用户级 `~/.teamfly/env.toml`
///   2. 项目级 `<工作目录>/.teamfly/env.toml`  ← 同名 key 覆盖用户级
/// 两者都可选,都没有 = 空。
pub fn load(teamfly_dir: &Path) -> Result<AgentEnv> {
    let mut env = AgentEnv::default();
    // 用户级
    if let Some(user_path) = user_env_path() {
        if user_path.exists() {
            merge_into(&mut env, &user_path)?;
        }
    }
    // 项目级(覆盖用户级)
    let project_path = teamfly_dir.join("env.toml");
    if project_path.exists() {
        merge_into(&mut env, &project_path)?;
    }
    Ok(env)
}

/// 用户级 env.toml 路径:~/.teamfly/env.toml
fn user_env_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".teamfly").join("env.toml"))
}

/// 把一个 env.toml 文件的内容合并进 env(同名 key 覆盖)。
fn merge_into(env: &mut AgentEnv, path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 {}", path.display()))?;
    let table: toml::Table = toml::from_str(&content)
        .with_context(|| format!("解析 {}", path.display()))?;

    for (k, v) in table {
        // backend 段
        if SECTION_KEYS.contains(&k.as_str()) {
            if let toml::Value::Table(sub) = v {
                for (kk, vv) in sub {
                    if let Some(s) = scalar_to_string(vv) {
                        match k.as_str() {
                            "claude" => { env.claude.insert(kk, expand(&s)); }
                            "codex" => { env.codex.insert(kk, expand(&s)); }
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }
        // 顶层标量 = 全局
        if let Some(s) = scalar_to_string(v) {
            env.global.insert(k, expand(&s));
        }
    }
    Ok(())
}

fn scalar_to_string(v: toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 展开 `$VAR` 和 `${VAR}` 为当前进程环境变量的值(未定义则保留原样)。
fn expand(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        // ${VAR}
        if chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for n in chars.by_ref() {
                if n == '}' {
                    break;
                }
                name.push(n);
            }
            match std::env::var(&name) {
                Ok(v) => out.push_str(&v),
                Err(_) => {
                    out.push_str("${");
                    out.push_str(&name);
                    out.push('}');
                }
            }
        } else {
            // $VAR
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_ascii_alphanumeric() || n == '_' {
                    name.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                out.push('$');
            } else {
                match std::env::var(&name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('$');
                        out.push_str(&name);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_braces() {
        std::env::set_var("TF_TEST_A", "hello");
        assert_eq!(expand("${TF_TEST_A}-world"), "hello-world");
        std::env::remove_var("TF_TEST_A");
    }

    #[test]
    fn expand_bare() {
        std::env::set_var("TF_TEST_B", "yo");
        assert_eq!(expand("prefix-$TF_TEST_B/x"), "prefix-yo/x");
        std::env::remove_var("TF_TEST_B");
    }

    #[test]
    fn expand_missing_kept() {
        std::env::remove_var("TF_ABSENT_XYZ");
        assert_eq!(expand("${TF_ABSENT_XYZ}!"), "${TF_ABSENT_XYZ}!");
        assert_eq!(expand("$TF_ABSENT_XYZ!"), "$TF_ABSENT_XYZ!");
    }

    #[test]
    fn expand_literal_dollar() {
        assert_eq!(expand("cost $5"), "cost $5"); // $后跟数字,不当变量
    }

    fn m(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn merged_claude_overrides_global() {
        let env = AgentEnv::from_maps(
            m(&[("LOG", "info"), ("ANTHROPIC_API_KEY", "global-key")]),
            m(&[("ANTHROPIC_API_KEY", "claude-key"), ("EXTRA", "c")]),
            HashMap::new(),
        );
        let merged = env.merged_for(BackendKind::Claude);
        assert_eq!(merged.get("LOG").unwrap(), "info");                 // 继承全局
        assert_eq!(merged.get("ANTHROPIC_API_KEY").unwrap(), "claude-key"); // 覆盖
        assert_eq!(merged.get("EXTRA").unwrap(), "c");                  // 段内独有
    }

    #[test]
    fn merged_codex_ignores_claude() {
        let env = AgentEnv::from_maps(
            HashMap::new(),
            m(&[("ANTHROPIC_API_KEY", "should-not-see")]),
            m(&[("OPENAI_API_KEY", "codex-key")]),
        );
        let merged = env.merged_for(BackendKind::Codex);
        assert!(!merged.contains_key("ANTHROPIC_API_KEY")); // claude 段不影响 codex
        assert_eq!(merged.get("OPENAI_API_KEY").unwrap(), "codex-key");
    }
}
