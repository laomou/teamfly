//! 辅助:写私密文件、seed MCP 骨架、seed codex provider。
//! 不再加载 env.toml —— 环境变量直接继承自 shell 进程。

use anyhow::{Context, Result};
use std::path::PathBuf;

/// 确保 ~/.codex/config.toml 里有 _tf provider。
/// 从环境变量 OPENAI_BASE_URL 取中转站地址。一次写入永久保留。
pub fn seed_codex_provider() -> Result<bool> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")));
    let Some(codex_cfg) = codex_home.map(|p| p.join("config.toml")) else {
        return Ok(false);
    };
    let base_url = match std::env::var("OPENAI_BASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => return Ok(false),
    };
    let base_url = if base_url.ends_with("/v1") {
        base_url
    } else if base_url.ends_with('/') {
        format!("{base_url}v1")
    } else {
        format!("{base_url}/v1")
    };
    let mut cfg: toml::Table = match std::fs::read_to_string(&codex_cfg) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => toml::Table::new(),
    };
    if let Some(providers) = cfg.get("model_providers").and_then(|v| v.as_table()) {
        if providers.contains_key("_tf") {
            return Ok(false);
        }
    }
    let mut provider = toml::Table::new();
    provider.insert("name".into(), toml::Value::String("AgentFly".into()));
    provider.insert("base_url".into(), toml::Value::String(base_url));
    provider.insert("env_key".into(), toml::Value::String("OPENAI_API_KEY".into()));
    provider.insert("wire_api".into(), toml::Value::String("responses".into()));
    cfg.entry("model_providers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert("_tf".to_string(), toml::Value::Table(provider));
    cfg.insert("model_provider".to_string(), toml::Value::String("_tf".to_string()));
    std::fs::write(&codex_cfg, toml::to_string_pretty(&cfg).unwrap())
        .with_context(|| format!("写入 {}", codex_cfg.display()))?;
    Ok(true)
}
