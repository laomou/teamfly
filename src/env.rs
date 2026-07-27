//! .teamfly/env.toml —— 全队共享的 agent 环境变量,注入到每个 agent 子进程。
//!
//! 格式(平铺 key=value):
//! ```toml
//! ANTHROPIC_BASE_URL = "https://中转站.example.com"
//! ANTHROPIC_API_KEY  = "sk-..."
//! ```
//! 值支持 `$VAR` / `${VAR}` 引用当前进程环境变量(便于把 key 存在 shell 里)。

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// 加载 .teamfly/env.toml。文件不存在返回空 map。
pub fn load(teamfly_dir: &Path) -> Result<HashMap<String, String>> {
    let path = teamfly_dir.join("env.toml");
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 {}", path.display()))?;
    let table: toml::Table = toml::from_str(&content)
        .with_context(|| format!("解析 {}", path.display()))?;

    let mut out = HashMap::new();
    for (k, v) in table {
        let raw = match v {
            toml::Value::String(s) => s,
            toml::Value::Integer(i) => i.to_string(),
            toml::Value::Boolean(b) => b.to_string(),
            _ => continue, // 只支持标量值
        };
        out.insert(k, expand(&raw));
    }
    Ok(out)
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
}
