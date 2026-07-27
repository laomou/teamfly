//! provider 配置:读 providers.toml,解析 base_url / api_key_env / protocol。

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProviderCfg {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_protocol")]
    #[allow(dead_code)] // 预留:当前只认 anthropic,未来区分协议
    pub protocol: String,
}

fn default_protocol() -> String {
    "anthropic".to_string()
}

#[derive(Debug, Default, serde::Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    provider: HashMap<String, ProviderCfg>,
}

#[derive(Debug, Default, Clone)]
pub struct Providers {
    pub map: HashMap<String, ProviderCfg>,
}

impl Providers {
    /// 依次尝试 <work>/.teamfly/providers.toml 与 ~/.teamfly/providers.toml,合并。
    pub fn load(teamfly_dir: &Path) -> Result<Self> {
        let mut map = HashMap::new();
        let candidates = [
            teamfly_dir.join("providers.toml"),
            home_teamfly().map(|p| p.join("providers.toml")).unwrap_or_default(),
        ];
        for path in candidates.iter() {
            if path.exists() {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("读取 {}", path.display()))?;
                let pf: ProvidersFile = toml::from_str(&content)
                    .with_context(|| format!("解析 {}", path.display()))?;
                for (k, v) in pf.provider {
                    map.entry(k).or_insert(v);
                }
            }
        }
        Ok(Providers { map })
    }

    pub fn get(&self, name: &str) -> Option<&ProviderCfg> {
        self.map.get(name)
    }

    /// 解析某 provider 的 base_url + api key(从环境变量读)。
    pub fn resolve(&self, name: &str) -> Option<(Option<String>, Option<String>)> {
        let cfg = self.get(name)?;
        let key = cfg
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok());
        Some((cfg.base_url.clone(), key))
    }
}

fn home_teamfly() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".teamfly"))
}
