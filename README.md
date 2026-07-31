# teamfly

[中文](README_zh.md)

An AI team-collaboration console in your terminal — **you lead a team, the agents are your teammates**. In a full-screen UI you set goals, `@`-mention people, and watch teammates work autonomously, hands-off.

> Toss out a goal → teammates run with it fully autonomously → you watch progress in the overview / per-member views and can `@` to step in anytime, but you're never blocked.

## Quick start

```bash
cargo build --release

# Get to work in a directory (defaults to the built-in `default` team: TPM + DEV + REV)
./target/release/teamfly work [work-dir] [--team <team-name>]
```

Once inside:

- Type in the bottom input box. **Work is dispatched only with `@name`**; the default team starts from `@TPM add a login feature`, where TPM breaks it down and schedules implementation and review. Without `@` it's just a note.
- `↑↓` (or click the left column) to switch between `# Overview` (all-hands timeline) and a member (the raw output stream of their process).
- `?` help · `^N` new topic · `^W` close topic · `Alt+1-9` switch topic · `PgUp`/`PgDn` scroll history · `Esc` back to overview.
- `^P` lift the anti-ping-pong pause and release queued work; `^C` cancels running agents first (press again to quit).
- Slash commands: `/team <name>` hot-swaps the team. Note it's **global** (all topics share one roster) and cancels running agents.

## A team = a folder on disk

There's no "create team" command; a team is just a folder under `.teamfly/teams/<name>/`. Edit the files to customize:

```
default/
├─ team.md            # team name + shared rules + team responsibilities + task hand-off (spliced into every agent)
└─ agents/
   ├─ TPM.md          # frontmatter (name/role/emoji/backend/model/read_only) + scheduling duties
   ├─ DEV.md          # implementation and testing
   └─ REV.md          # code review only
```

- **agent md** states a single responsibility (who I am, what I do); **team.md** states team responsibilities and task hand-off (who passes to whom on completion) — change the flow in one place.
- `backend` is one of two: `claude` (claude CLI, stream-json) / `codex` (codex CLI, JSONL).
- `model` is optional. If set, it's passed as `--model` to that member's CLI, so **you can swap the model per member** (e.g. a cheap one for review, a strong one for implementation); if unset, nothing is passed and the CLI decides per its own config.
- `read_only` is optional (default `false` = writable). A member marked `read_only: true` runs in the **main work dir** with **genuinely no write permission** (claude via `--permission-mode plan`, codex via `--sandbox read-only`), suitable for review/scheduling roles that shouldn't touch files. In the built-in team TPM/REV are read-only, DEV is writable.
- The built-in `default` team (TPM/DEV/REV) is auto-seeded into the work dir's `.teamfly/teams/default/` on first run; an old unmodified `team.md` / `QE.md` is auto-migrated (`DEV.md` is not migrated — delete it and restart if you want the new persona).

## Credentials and models

teamfly **doesn't manage these** — agent subprocesses directly inherit the environment variables from your shell, and `claude` / `codex` each read their own config:

```bash
export ANTHROPIC_BASE_URL=https://api.anthropic.com
export ANTHROPIC_AUTH_TOKEN=...

export OPENAI_API_KEY=...                   # codex members
```

Configure proxies and auth however you like, exactly as when you use the `claude` / `codex` CLIs directly — teamfly doesn't insert a layer in between.

Models can be specified two ways, **per-member** takes priority:

| Way | Granularity | How |
|---|---|---|
| `model:` in agent md | single member | write in frontmatter, passed as `--model` to that member |
| the CLI's own config | whole team | `ANTHROPIC_MODEL` env var / codex's config |

When agent md has no `model:`, teamfly passes no `--model` at all and the CLI uses its own settings.

## MCP

When `.teamfly/mcp.json` exists in the work dir, it's passed to agents as `--mcp-config`; if absent, nothing is passed.

## How it works

- **Restarted each round, stateless**: on `@`, a new subprocess starts, does the work, emits a final reply, and exits.
- **Acts only when `@`-ed, stops when done**: idle by default, `@`-driven. With no `@`, everyone stays quiet.
- **Reporting**: an agent's final reply for a round (claude/codex's result) goes into the overview automatically; `@name` is parsed from it and delivered to the corresponding member.
- **Context**: the overview timeline serves double duty — it's both the UI and the "incremental backstory" fed to an awakened agent.
- **Topics (tabs)**: belong to the project, persisted at `.teamfly/issues/<id>-<name>.jsonl`, auto-restored on reopen. Empty topics aren't persisted. Filenames carry an id so topic ids stay stable across restarts (worktree dirs and branches are named by it); old files without an id prefix are auto-migrated. Issued ids are tracked in `.teamfly/next-issue-id` — closing a topic deletes its jsonl but **keeps the branch**, so if an id gets recycled and reissued, the new topic checks out the previous topic's branch.
- **@-ed while busy → queued**, not interrupted. Anti-ping-pong: an over-deep `@` chain auto-pauses (`^P` resumes).
- **Auto-retry on failure** 3 times (for proxy 429/5xx); still failing, surfaces as a system dropped-off message.

## Agent change isolation (worktree)

When the work dir is a git repo, each **topic** gets its own worktree and branch, so agent changes don't land in your work dir:

```
branch  teamfly/issue-<topic-id>
dir     .teamfly/worktrees/<topic-id>/
```

- The relay within a topic (TPM → DEV → REV) shares this worktree; downstream sees the files upstream changed directly, no branch merge needed.
- On handoff the report appends a line `📂 teamfly/issue-3 — committed 2 files changed … · uncommitted …`.
- It's just an ordinary branch in the repo, handle it however: `git push origin teamfly/issue-3` to open an MR/PR, `git merge` locally, `git cherry-pick` to take only part, or `git diff main..teamfly/issue-3` to look first.
- Discard: `git branch -D teamfly/issue-3` (or just ignore it). The worktree is auto-reclaimed when the whole topic hasn't changed a single character.
- Closing a topic (`^W`) **doesn't touch the branch** — closing just means "not looking anymore", it doesn't destroy the work; the worktree dir is only reclaimed when there are no uncommitted changes, otherwise it's kept and noted in the message.
- On startup, if `.teamfly/` isn't git-ignored, teamfly **auto-appends a `.teamfly/` line to the project `.gitignore`** and reports it in the pre-check — because `.teamfly/` holds topic history and `mcp.json` (which may contain auth headers), and without ignoring, one `git add -A` from an agent would commit them. If your `.gitignore` already has a `.teamfly/` rule (even negated with `!.teamfly/`), it's left untouched.
- When the work dir isn't a git repo the whole isolation is unavailable, falling back to all agents sharing the work dir (warned on startup).

## Architecture

Hand-written TEA-like: a single `Model` + `Msg` enum + centralized `update` + `view`. All tokio concurrent event sources funnel into one `mpsc<Msg>`, the main loop feeds them to `update` one by one — lock-free, race-free. Side effects are returned by `update` as a `Command`, executed by the runtime, then posted back as a `Msg`.

Modules: `cli` · `team` · `backend` · `stream` · `router` · `issue` · `builtin` · `slash` · `tui` · `app` · `model`.

## Tests

```bash
cargo test
```

Pure-function unit tests (report distillation / @ parsing / ANSI stripping / claude+codex event parsing) plus keyboard-operation and topic add/remove tests.

## Known boundaries

- **Writable** members within the same topic run serially (they share one worktree; changing files simultaneously would clobber each other); across topics they run in parallel.
- Not done: adding/removing members at runtime, `@all`, multiple instances of the same role.
- Built-in team files are UTF-8 without BOM; keep UTF-8 when editing agent md, don't save as GBK.
