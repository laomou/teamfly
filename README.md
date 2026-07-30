# teamfly

终端里的 AI 团队协作台 —— **你带一支队,agent 是队员**。在全屏界面里发目标、`@` 圈人,看队员放养式自动干活。

> 甩一句目标 → 队员全自动开干 → 你在总览/单人视图里看进展、随时可 `@` 插手,但从不被拦。

## 快速开始

```bash
cargo build --release

# 在工作目录里开干(缺省用内置 default 队:TPM + DEV + REV)
./target/release/teamfly work [工作目录] [--team <团队名>]
```

进入后:

- 底部输入框打字。**带 `@名字` 才会派活**；默认团队从 `@TPM 加个登录功能` 开始，由 TPM 拆解、调度实现和评审。不带 `@` 只是留言。
- `↑↓`(或鼠标点左栏)在 `# 总览`(全员时间线)/ 某个成员(看他进程的原始输出流)间切。
- `?` 帮助 · `^N` 新议题 · `^W` 关议题 · `Alt+1-9` 切议题 · `PgUp`/`PgDn` 翻历史 · `Esc` 回总览。
- `^P` 解除防乒乓暂停并放出排队的活;`^C` 在有 agent 在跑时先取消它们(再按一次才退出)。
- 斜杠命令:`/team <名>` 热切团队。注意是**全局**生效(所有议题共用一份花名册),且会取消正在跑的 agent。

## 团队 = 磁盘文件夹

无造队命令,团队就是 `.teamfly/teams/<名>/` 下的一个文件夹,改文件即自定义:

```
default/
├─ team.md            # 队名 + 全员规矩 + 团队职责 + 任务流转(拼进每个 agent)
└─ agents/
   ├─ TPM.md          # frontmatter(name/role/emoji/backend/model/read_only) + 调度职责
   ├─ DEV.md          # 实现与测试
   └─ REV.md          # 只做代码评审
```

- **agent md** 只写单一职责(我是谁、做什么);**team.md** 写团队职责和任务流转(谁完成后交给谁),改流程只改一处。
- `backend` 二选一:`claude`(claude CLI,stream-json)/ `codex`(codex CLI,JSONL)。
- `model` 可选。写了就作为 `--model` 传给该成员的 CLI,**可以给单个成员单独换模型**
  (比如评审用便宜的、实现用强的);不写则完全不传,由 CLI 自己按它的配置决定。
- `read_only` 可选(默认 `false` = 可写)。写 `read_only: true` 的成员在**主工作目录**里跑且**真的没有写权限**
  (claude 走 `--permission-mode plan`,codex 走 `--sandbox read-only`),适合评审、调度这类不该改文件的角色。
  内置队里 TPM/REV 是只读,DEV 可写。
- 内置 `default` 队(TPM/DEV/REV)首次运行自动播种到工作目录的 `.teamfly/teams/default/`；旧的未修改 `team.md` / `QE.md` 会自动迁移(`DEV.md` 不迁移,想拿新版人设请自行删掉它再启动)。

## 凭证与模型

teamfly **不管这些** —— agent 子进程直接继承你 shell 里的环境变量,
`claude` / `codex` 各自读自己那套配置:

```bash
export ANTHROPIC_BASE_URL=https://api.anthropic.com
export ANTHROPIC_AUTH_TOKEN=...

export OPENAI_API_KEY=...                   # codex 成员
```

中转站、鉴权方式想怎么配就怎么配,和你平时直接用 `claude` / `codex` 命令行时
完全一样,teamfly 不在中间插一层。

模型有两种指定方式,**按成员** 优先:

| 方式 | 粒度 | 怎么写 |
|---|---|---|
| agent md 的 `model:` | 单个成员 | frontmatter 里写,作为 `--model` 传给该成员 |
| CLI 自己的配置 | 全队 | `ANTHROPIC_MODEL` 环境变量 / codex 的 config |

agent md 里没写 `model:` 时 teamfly 完全不传 `--model`,CLI 就按自己那套来。

## MCP

工作目录下的 `.teamfly/mcp.json` 存在时会作为 `--mcp-config` 传给 agent,
不存在就不传。

## 工作机制

- **每轮重起、无状态**:被 `@` 时起一个新子进程,干完输出最终回复就退。
- **只被 `@` 才干,干完即停**:默认静止,`@` 驱动。无人 `@` 则全体安静。
- **汇报**:agent 一轮的最终回复(claude/codex 的 result)自动进总览;`@名字` 从中解析,投递给对应成员。
- **上下文**:总览时间线一物两用——既是 UI,又作为「增量前情」喂给被唤醒的 agent。
- **议题(tab)**:属于项目,落盘 `.teamfly/issues/<id>-<名>.jsonl`,关掉重开自动恢复。空议题不落盘。
  文件名带 id 是为了让议题 id 跨重启稳定(worktree 目录和分支都按它命名);旧的无 id 前缀的文件会自动迁移。
  已发放的 id 记在 `.teamfly/next-issue-id` 里 —— 关议题会删掉它的 jsonl 但**保留分支**,
  id 要是被回收重发,新议题就会 checkout 到上一个议题的分支上。
- **忙时被 @ → 排队**,不打断。防乒乓:`@` 连锁过深自动暂停(`^P` 恢复)。
- **失败自动重试** 3 次(应对中转站 429/5xx),仍失败则作为系统消息掉线提示。

## agent 改动隔离(worktree)

工作目录是 git 库时,每个**议题**有自己的 worktree 和分支,agent 的改动不落进你的工作目录:

```
分支  teamfly/issue-<议题id>
目录  .teamfly/worktrees/<议题id>/
```

- 同议题内的接力(TPM → DEV → REV)共享这个工作树,下游直接看得到上游改的文件,不需要合分支。
- 交卷时汇报里会附一行 `📂 teamfly/issue-3 — 已提交 2 files changed … · 未提交 …`。
- 这就是仓库里一个普通分支,怎么处理随你:`git push origin teamfly/issue-3` 推上去开 MR/PR、
  `git merge` 本地合、`git cherry-pick` 只挑一部分、或者 `git diff main..teamfly/issue-3` 先看看。
- 丢弃:`git branch -D teamfly/issue-3`(或直接不理它)。整个议题一个字都没改过时 worktree 会被自动回收。
- 关闭议题(`^W`)**不动分支** —— 关掉只是「不看了」,不销毁工作成果;worktree 目录只在没有
  未提交改动时才收掉,有未提交改动就一并留着并在提示里说明。
- 启动时若发现 `.teamfly/` 没被 git 忽略,teamfly 会**自动往项目 `.gitignore` 追加一条 `.teamfly/`**
  并在预检里告知 —— 因为 `.teamfly/` 下有议题历史和 `mcp.json`(可能含鉴权 header),不忽略的话
  agent 一句 `git add -A` 就把它们提交进去了。你的 `.gitignore` 里已经有 `.teamfly/` 规则(哪怕被 `!.teamfly/` 否定)时不会动它。
- 工作目录不是 git 库时整套隔离不可用,退回所有 agent 共用工作目录(启动时会警告)。

## 架构

手写 TEA-like:单一 `Model` + `Msg` 枚举 + 集中 `update` + `view`。tokio 所有并发事件源汇成一条 `mpsc<Msg>`,主循环逐条喂 `update`,无锁无竞态。副作用由 `update` 返回 `Command`、runtime 执行后回投 `Msg`。

模块:`cli` · `team` · `backend` · `stream` · `router` · `issue` · `builtin` · `slash` · `tui` · `app` · `model`。

## 测试

```bash
cargo test
```

纯函数单测(汇报提炼/@ 解析/剥 ANSI/claude+codex 事件解析)+ 键盘操作与议题增删测试。

## 已知边界

- 同一议题内的**可写**成员串行执行(共享一个 worktree,同时改文件会互相踩);跨议题并行。
- 不做:运行时增删成员、`@all`、同角色多开。
- 内置团队文件是 UTF-8 无 BOM;用编辑器改 agent md 时保持 UTF-8,别存成 GBK。
