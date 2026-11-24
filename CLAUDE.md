<CRITICAL_INSTRUCTION>

## Tasks live on the board

This project's tasks are on the team board (Vercel Board, `vb`), channel `tasks`, not in
Backlog.md, which is retired: there is no `backlog/` and no `backlog` CLI to run.

Before any task action (finding, filing, claiming, noting, finishing), read AGENTS.md's
**Tasks: the board** and follow it: claim a task before working it, write as your worktree's
name (`--as "$(scripts/agent-name.py)"`: adjective-noun, never `claude`), cite tasks by board ID
(`board 3012`) or, for tasks from Backlog.md, by their `TASK-N`. AGENTS.md is how all work on
cladefs is done; read the rest of it too.

## On-disk formats are frozen

All persistent on-disk and bucket formats are now frozen (board 10033, `formats/`). **Any** change
to a frozen format requires **explicit user approval directly in conversation** (not a hotbed
question), and **backwards compatibility with all stored data is required** — every later build must
read all previously stored versions. Read AGENTS.md's **Frozen formats** before touching any format.

</CRITICAL_INSTRUCTION>
