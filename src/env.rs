//! env.toml —— agent 环境变量,注入到 agent 子进程。
//!
//! 两级加载(**不合并**,项目级整体顶替用户级):
//!   1. 用户级 `~/.teamfly/env.toml` —— API key 等全局默认放这里,一次配好多项目共用
//!   2. 项目级 `<工作目录>/.teamfly/env.toml` —— 存在则**整体顶替**用户级(不逐 key 继承),
//!      所以项目级里要把 token 之类也一并写全
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
use std::path::{Path, PathBuf};

/// backend 段名。
const SECTION_KEYS: &[&str] = &["claude", "codex"];

#[derive(Debug, Default, Clone)]
pub struct AgentEnv {
    global: HashMap<String, String>,
    claude: HashMap<String, String>,
    codex: HashMap<String, String>,
    /// 未在当前进程环境里展开的 ${VAR} / $VAR 名字集合(供预检 warn)。
    pub unresolved: Vec<String>,
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
        AgentEnv { global, claude, codex, unresolved: Vec::new() }
    }
}

/// 加载 env.toml。**不合并**:
///   项目级 `<工作目录>/.teamfly/env.toml` 存在 → 只用项目级(忽略用户级)
///   否则 → 用用户级 `~/.teamfly/env.toml`(不存在则自动 seed 模板)
pub fn load(teamfly_dir: &Path) -> Result<AgentEnv> {
    let mut env = AgentEnv::default();

    // 项目级存在 → 只用它
    let project_path = teamfly_dir.join("env.toml");
    if project_path.exists() {
        merge_into(&mut env, &project_path)?;
        return Ok(env);
    }

    // 否则用用户级(不存在则播种模板)
    if let Some(user_path) = user_env_path() {
        seed_user_env(&user_path).ok(); // seed 失败不致命
        if user_path.exists() {
            merge_into(&mut env, &user_path)?;
        }
    }
    Ok(env)
}

/// 用户级 env.toml 路径:~/.teamfly/env.toml
pub fn user_env_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".teamfly").join("env.toml"))
}

/// 用户级 mcp.json 路径:~/.teamfly/mcp.json
pub fn user_mcp_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".teamfly").join("mcp.json"))
}

/// 私密地写一个新文件:创建时就带 0600,不存在「先 0644 再 chmod」的可读窗口。
/// env.toml / mcp.json 是要放 API key 的地方,不能落成默认的 0644 ——
/// 同机任何用户都能 cat 走。
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 先写到同目录的临时文件再 rename:写一半失败不会把原文件截断成半截
    let tmp = path.with_extension("tmp.new");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("创建 {}", tmp.display()))?;
        use std::io::Write;
        f.write_all(content.as_bytes())
            .with_context(|| format!("写入 {}", tmp.display()))?;
        f.sync_all().ok();
    }
    // 临时文件已存在时 mode 不会重设,补一次(仅 unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置 {} 权限", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("替换 {}", path.display()))?;
    Ok(())
}

/// 若用户级 env.toml 不存在,创建一个带注释的模板。返回是否新建。
pub fn seed_user_env(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let tmpl = r#"# teamfly 用户级 agent 环境变量(全局默认,所有项目共用)
# 注意:项目里若有 <工作目录>/.teamfly/env.toml,它会**整体顶替**这个文件,
# 不是逐 key 覆盖 —— 项目级里没写的 key(比如 token)不会从这里继承。
# 值支持 ${VAR} 或 $VAR 引用当前 shell 里的环境变量(避免密钥入文件)
#
# 常见用法:把 key 存在 shell(比如 ~/.zshrc),这里只写引用:
#
# [claude]
# ANTHROPIC_BASE_URL   = "https://api.anthropic.com"
# ANTHROPIC_AUTH_TOKEN = "${ANTHROPIC_AUTH_TOKEN}"
#
# [codex]
# OPENAI_API_KEY = "${OPENAI_API_KEY}"

# —— 取消下面注释、按需修改即可 ——

# [claude]
# ANTHROPIC_BASE_URL   = "https://api.anthropic.com"
# ANTHROPIC_AUTH_TOKEN = "${ANTHROPIC_AUTH_TOKEN}"
"#;
    // 这个模板正文就在教用户把 token 填进来,所以从创建那一刻就得是 0600
    write_private(path, tmpl)?;
    Ok(true)
}

/// 确保 ~/.codex/config.toml 里有 _tf provider。
///
/// 用户配了 OPENAI_BASE_URL 但 codex 不认识它,需要写一个 provider 定义进去。
/// 写到 ~/.codex/config.toml(不是项目配置),一劳永逸,codex 自己就能用。
pub fn seed_codex_provider(teamfly_dir: &Path) -> Result<bool> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(".codex"))
        });
    let Some(codex_cfg) = codex_home.map(|p| p.join("config.toml")) else {
        return Ok(false);
    };

    // 读项目级的 env.toml 取 BASE_URL
    let project_env = teamfly_dir.join("env.toml");
    let mut base_url = String::new();
    if let Ok(text) = std::fs::read_to_string(&project_env) {
        if let Ok(table) = text.parse::<toml::Table>() {
            if let Some(codex_tbl) = table.get("codex").and_then(|v| v.as_table()) {
                if let Some(url) = codex_tbl.get("OPENAI_BASE_URL").and_then(|v| v.as_str()) {
                    base_url = url.to_string();
                }
            }
        }
    }
    // 回退到用户级
    if base_url.is_empty() {
        if let Some(user_path) = user_env_path() {
            if let Ok(text) = std::fs::read_to_string(&user_path) {
                if let Ok(table) = text.parse::<toml::Table>() {
                    if let Some(codex_tbl) = table.get("codex").and_then(|v| v.as_table()) {
                        if let Some(url) = codex_tbl.get("OPENAI_BASE_URL").and_then(|v| v.as_str()) {
                            base_url = url.to_string();
                        }
                    }
                }
            }
        }
    }
    if base_url.is_empty() {
        return Ok(false);
    }

    // normalize: 补 /v1 后缀
    if !base_url.ends_with("/v1") {
        if base_url.ends_with('/') {
            base_url.push_str("v1");
        } else {
            base_url.push_str("/v1");
        }
    }

    // 读现有 codex config,合并 _tf provider
    let mut cfg: toml::Table = match std::fs::read_to_string(&codex_cfg) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => toml::Table::new(),
    };

    // 已经有 _tf 了 → 不动
    if let Some(providers) = cfg.get("model_providers").and_then(|v| v.as_table()) {
        if providers.contains_key("_tf") {
            return Ok(false);
        }
    }

    // 加 _tf provider
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
    // 不设默认 model_provider —— 用户可能已有自己的 provider(比如 stepcode)。
    // 加了 _tf 定义后,codex_cmd 可以用 -c model_provider=_tf 来选它,
    // 不影响用户已有的默认配置。

    // 写回去
    // 这里不写 0600,codex config.toml 不含明文 key(key 从 env 拿)
    std::fs::write(&codex_cfg, toml::to_string_pretty(&cfg).unwrap())
        .with_context(|| format!("写入 {}", codex_cfg.display()))?;
    Ok(true)
}

/// 若用户级 mcp.json 不存在,创建一个空的 mcpServers 骨架。返回是否新建。
pub fn seed_user_mcp(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    // 只写空 mcpServers,新加 server 时把配置塞进这个对象即可
    // (MCP server 配置里常带鉴权 header,一并按私密文件处理)
    write_private(path, "{\n  \"mcpServers\": {}\n}\n")?;
    Ok(true)
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
                        let expanded = expand(&s, &mut env.unresolved);
                        match k.as_str() {
                            "claude" => { env.claude.insert(kk, expanded); }
                            "codex" => { env.codex.insert(kk, expanded); }
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }
        // 顶层标量 = 全局
        if let Some(s) = scalar_to_string(v) {
            let expanded = expand(&s, &mut env.unresolved);
            env.global.insert(k, expanded);
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

/// 展开 `$VAR` 和 `${VAR}` 为当前进程环境变量的值(未定义则保留原样,并记入 unresolved)。
fn expand(s: &str, unresolved: &mut Vec<String>) -> String {
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
                    if !name.is_empty() && !unresolved.contains(&name) {
                        unresolved.push(name.clone());
                    }
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
                        if !unresolved.contains(&name) {
                            unresolved.push(name.clone());
                        }
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
        assert_eq!(expand("${TF_TEST_A}-world", &mut Vec::new()), "hello-world");
        std::env::remove_var("TF_TEST_A");
    }

    #[test]
    fn expand_bare() {
        std::env::set_var("TF_TEST_B", "yo");
        assert_eq!(expand("prefix-$TF_TEST_B/x", &mut Vec::new()), "prefix-yo/x");
        std::env::remove_var("TF_TEST_B");
    }

    #[test]
    fn expand_missing_kept() {
        std::env::remove_var("TF_ABSENT_XYZ");
        assert_eq!(expand("${TF_ABSENT_XYZ}!", &mut Vec::new()), "${TF_ABSENT_XYZ}!");
        assert_eq!(expand("$TF_ABSENT_XYZ!", &mut Vec::new()), "$TF_ABSENT_XYZ!");
    }

    #[test]
    fn expand_literal_dollar() {
        assert_eq!(expand("cost $5", &mut Vec::new()), "cost $5"); // $后跟数字,不当变量
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
