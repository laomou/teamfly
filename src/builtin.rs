//! 内置默认团队:嵌进二进制,首次运行时播种到 <工作目录>/.teamfly/teams/default。

use anyhow::{Context, Result};
use std::path::Path;

pub const DEFAULT_TEAM: &str = "default";

// v0.1 默认团队的内容。仅当用户的文件仍与旧内置版本完全一致时才自动迁移，
// 避免覆盖用户的自定义团队。
const LEGACY_TEAM: &str = r#"---
name: default
---
你在一个终端协作台里,和「我」(负责人)以及其他成员一起干活。

## 全员规矩
- 你在指定的工作目录里干活,可直接读写文件、跑命令,无需请求许可。
- 每轮结束时,用简短一段话总结你做了什么、结果如何。
- 汇报要短、说人话,细节留在你自己的输出里。
- 需要谁接力,就在结尾 @他的名字(只能 @ 团队里已有的人),后面留个空格。

## 团队职责
- DEV(开发):实现功能、改代码。
- QE(测试):补测试、评审、挑问题。

## 任务流转
- DEV 实现完一个功能后,@QE 请他测试/评审。
- QE 测出问题后,@DEV 请他修。
- 无需接力(活已彻底完成或只是回答问题)时,不 @ 任何人。
"#;

const LEGACY_QE: &str = r#"---
name: QE
role: 测试
emoji: "🧪"
backend: claude
---
你是 QE,负责质量。别人实现完后你补测试并跑一遍,也做基本评审挑问题。
只管做好测试评审这一件事;要不要交给别人、交给谁,按团队的「任务流转」规则来。
"#;

/// (相对路径, 文件内容) —— 默认团队的全部文件(编译期从 src/assets 嵌入)。
const FILES: &[(&str, &str)] = &[
    ("team.md", include_str!("assets/teams/default/team.md")),
    (
        "agents/TPM.md",
        include_str!("assets/teams/default/agents/TPM.md"),
    ),
    (
        "agents/DEV.md",
        include_str!("assets/teams/default/agents/DEV.md"),
    ),
    (
        "agents/REV.md",
        include_str!("assets/teams/default/agents/REV.md"),
    ),
];

/// 播种默认团队。
///
/// **只在目录完整时不动**;任何一个内置文件缺失都重新补上。
/// 这是默认团队,用户想自定义应该复制一份改名,而不是在 default 上删文件。
///
/// 例外:旧 DEV/QE 默认团队迁移时同理补齐新角色。
pub fn seed_default(teamfly_dir: &Path) -> Result<()> {
    let root = teamfly_dir.join("teams").join(DEFAULT_TEAM);
    migrate_legacy_default(&root)?;
    for (rel, content) in FILES {
        let path = root.join(rel);
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content).with_context(|| format!("写入 {}", path.display()))?;
    }
    Ok(())
}

fn migrate_legacy_default(root: &Path) -> Result<()> {
    let team_path = root.join("team.md");
    replace_if_unmodified(&team_path, LEGACY_TEAM, FILES[0].1)?;

    let qe_path = root.join("agents/QE.md");
    match std::fs::read_to_string(&qe_path) {
        Ok(content) if content == LEGACY_QE => {
            std::fs::remove_file(&qe_path)
                .with_context(|| format!("删除 {}", qe_path.display()))?;
        }
        Ok(_) => {
            eprintln!(
                "teamfly: 保留已自定义的旧默认成员 {}; 请手动迁移或删除它。",
                qe_path.display()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("读取 {}", qe_path.display())),
    }
    Ok(())
}

/// 内容与 `old` 逐字节相同(= 用户没改过)才替换成 `new`。返回是否替换了。
fn replace_if_unmodified(path: &Path, old: &str, new: &str) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(content) if content == old => {
            std::fs::write(path, new).with_context(|| format!("迁移 {}", path.display()))?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("读取 {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("teamfly-builtin-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn legacy_default_team_is_migrated() {
        let dir = temp_dir("migrate");
        let root = dir.join("teams/default");
        std::fs::create_dir_all(root.join("agents")).unwrap();
        std::fs::write(root.join("team.md"), LEGACY_TEAM).unwrap();
        std::fs::write(root.join("agents/QE.md"), LEGACY_QE).unwrap();

        seed_default(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("team.md")).unwrap(),
            FILES[0].1
        );
        assert!(!root.join("agents/QE.md").exists());
        assert!(root.join("agents/TPM.md").exists());
        assert!(root.join("agents/DEV.md").exists());
        assert!(root.join("agents/REV.md").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn customized_legacy_files_are_preserved() {
        let dir = temp_dir("preserve");
        let root = dir.join("teams/default");
        std::fs::create_dir_all(root.join("agents")).unwrap();
        std::fs::write(root.join("team.md"), "my team").unwrap();
        std::fs::write(root.join("agents/QE.md"), "my QE").unwrap();

        seed_default(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("team.md")).unwrap(),
            "my team"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("agents/QE.md")).unwrap(),
            "my QE"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
