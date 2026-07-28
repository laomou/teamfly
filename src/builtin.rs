//! 内置默认团队:嵌进二进制,首次运行时播种到 <工作目录>/.teamfly/teams/default。

use anyhow::{Context, Result};
use std::path::Path;

pub const DEFAULT_TEAM: &str = "default";

/// (相对路径, 文件内容) —— 默认团队的全部文件。
const FILES: &[(&str, &str)] = &[
    (
        "team.md",
        "---\nname: default\n---\n\
你在一个终端群聊里和「我」(群主)以及其他群友协作。\n\n\
全员规矩:\n\
- 你在指定的工作目录里干活,可直接读写文件、跑命令,无需请求许可。\n\
- 每轮结束时,用一行以「【群聊】」开头的话向群里汇报结论(干了什么、结果如何)。\n\
- 需要谁接力,就在【群聊】那行 @他的名字(只能 @ 群里已有的人)。\n\
- 汇报要短、说人话,细节留在你自己的输出里。\n",
    ),
    (
        "agents/DEV.md",
        "---\nname: DEV\nrole: 开发\nemoji: \"💻\"\nbackend: claude\nmodel: claude-opus-4-6\n---\n\
你是 DEV,负责实现。收到任务直接动手写代码、改文件,把功能做出来。\n\
完成后在【群聊】汇报改了什么、影响哪些文件。需要测试就 @QE。\n",
    ),
    (
        "agents/QE.md",
        "---\nname: QE\nrole: 测试\nemoji: \"🧪\"\nbackend: claude\nmodel: claude-opus-4-6\n---\n\
你是 QE,负责质量。别人实现完后你补测试并跑一遍,也做基本评审挑问题。\n\
【群聊】汇报覆盖了什么、是否全绿、有无隐患。\n",
    ),
];

/// 若 <teamfly_dir>/teams/default 不存在,则播种。已存在则不动(尊重用户改动)。
pub fn seed_default(teamfly_dir: &Path) -> Result<()> {
    let root = teamfly_dir.join("teams").join(DEFAULT_TEAM);
    if root.is_dir() {
        return Ok(());
    }
    for (rel, content) in FILES {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content).with_context(|| format!("写入 {}", path.display()))?;
    }
    Ok(())
}
