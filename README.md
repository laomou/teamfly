# teamfly

终端里的 AI 团队协作台 —— **你是我,agent 是群友**。在一个全屏群聊界面里发目标、`@` 圈人,看群友放养式自动干活。

> 甩一句目标 → 群友全自动开干 → 你在群聊/单人视图里看进展、随时可 `@` 插手,但从不被拦。

## 快速开始

```bash
cargo build --release
./target/release/teamfly work [工作目录] --team <团队文件夹>
# 缺省工作目录 = 当前目录;缺省团队 = ./teams/后端队
```

例:

```bash
./target/release/teamfly work ./sample --team ./teams/后端队
```

进入后:

- 在底部输入框打字。**带 `@名字` 才会派活**(如 `@老K 重构登录模块`);不带 `@` 只是留言。
- `↑↓` 或鼠标点击左栏:`# 群聊`(全员时间线)/ 某个成员(看他 backend 进程的原始输出流)。
- `Esc` 回群聊 · `^1`~`^9` 切议题 · `Ctrl+P` 解除防乒乓暂停 · `Ctrl+C` 退出。

## 团队 = 磁盘文件夹

无造队命令,团队就是一个文件夹,改文件即自定义:

```
后端队/
├─ team.md          # 群名 + 全员公共规矩 + 项目背景(拼进每个 agent)
└─ agents/
   ├─ 老K.md         # frontmatter(name/role/backend/model/provider) + 人设正文
   ├─ 阿码.md
   ├─ 小盾.md
   └─ 阿测.md
```

`backend` 三选一(静态写死):

- `claude` / `codex` — 起对应 CLI(headless,凭证透传环境;带跳过权限 flag)
- `api` — teamfly 自跑 Anthropic 原生 loop(端点/key 走 `.teamfly/providers.toml` 或 `~/.teamfly/providers.toml`)
- `mock` — 无凭证的确定性后端,供测试/演示

## 工作机制

- **每轮重起、无状态**:被 `@` 时起一个新进程,干完产出一句 `【群聊】…` 汇报就退。
- **只被 `@` 才干,干完即停**:默认静止,`@` 驱动。无人 `@` 则全体安静。
- **群上下文**:精炼的群聊时间线一物两用——既是 UI,又作为「增量前情」喂给被唤醒的 agent。
- **agent 互 `@`**:汇报里的 `@小盾` 会把消息投递给小盾进程,交接在群聊可见。忙时排队,不打断。防乒乓:`@` 连锁过深自动暂停。
- **落盘**:每个议题的时间线追加到 `<工作目录>/.teamfly/issues/<名>.jsonl`,关掉重开自动恢复。

## 架构

手写 TEA-like:单一 `Model` + `Msg` 枚举 + 集中 `update` + `view`。tokio 所有并发事件源汇成一条 `mpsc<Msg>`,主循环逐条喂 `update`,无锁无竞态。副作用由 `update` 返回 `Command`、runtime 执行后回投 `Msg`。

模块:`cli` · `team` · `provider` · `backend` · `router` · `issue` · `tui` · `app` · `model`。

## 测试

```bash
cargo test
```

含单元测试(路由/汇报提取/剥 ANSI/增量前情)与两个端到端测试(通过真实 TEA 循环 + mock 后端驱动完整 `@` 级联与落盘/重放)。

## 已知边界(MVP)

- 多 agent 并行改同一文件不做 git worktree 隔离(建议工作目录是 git 库)。
- 不做:运行时增删成员、`@all`、同角色多开、右侧抽屉、拍板/权限审批。
