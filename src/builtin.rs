//! 内置默认团队:嵌进二进制,首次运行时播种到 <工作目录>/.teamfly/teams/default。

use anyhow::{Context, Result};
use std::path::Path;

pub const DEFAULT_TEAM: &str = "default";

/// (相对路径, 文件内容) —— 默认团队的全部文件(编译期从 src/assets 嵌入)。
const FILES: &[(&str, &str)] = &[
    ("team.md", include_str!("assets/teams/default/team.md")),
    ("agents/TPM.md", include_str!("assets/teams/default/agents/TPM.md")),
    ("agents/DEV.md", include_str!("assets/teams/default/agents/DEV.md")),
    ("agents/REV.md", include_str!("assets/teams/default/agents/REV.md")),
];

/// 播种默认团队:目录不存在则完整播种;目录已存在但个别文件缺失,则补上缺失的。
/// 已存在的文件不覆盖(尊重用户改动)。
pub fn seed_default(teamfly_dir: &Path) -> Result<()> {
    let root = teamfly_dir.join("teams").join(DEFAULT_TEAM);
    for (rel, content) in FILES {
        let path = root.join(rel);
        if path.exists() {
            continue; // 已存在,不动
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content).with_context(|| format!("写入 {}", path.display()))?;
    }
    Ok(())
}
