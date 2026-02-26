# Claude Sessions (Rust)

`cs` is a fast terminal session browser and resume launcher for both Claude Code and Codex CLI sessions.

## Overview

- Supports two sources with parity:
  - Claude Code (`~/.claude/...`)
  - Codex CLI (`~/.codex/...`)
- Startup is cache-backed and optimized for large history files.
- Only resumable sessions are shown in list/TUI output.
- Missing, moved, deleted, or partially corrupt files are handled gracefully.
- Resume uses the most recently known model per source.
- Stats are source-separated (Claude Code and Codex are not mixed).

Data flow at startup:

```text
~/.claude/history.jsonl  ----\
                              +--> session index --> resumable filter --> list/TUI
~/.codex/history.jsonl   ----/            |
                                           +--> ~/.local/state/cs-rs/session-cache-v1.json
```

## Build

```bash
cargo build --release
```

Binary output:

```text
target/release/cs-rs
```

## Install Launcher Stub

`~/.local/bin/cs` should exec the release binary:

```bash
exec /Users/mbm-gsc/permanent/claude-sessions/target/release/cs-rs "$@"
```

## Usage

```bash
cs [command]
```

If no command is provided, `cs` opens the interactive TUI session list.

## Commands

### `cs list`

List sessions in plain text or JSON.

```bash
cs list [--project <text>] [--since YYYY-MM-DD] [--limit N] [--json]
```

### `cs view`

View a single session by ID (supports short IDs).

```bash
cs view <session-id> [--thinking] [--tail N] [--no-pager]
```

### `cs search`

Search session messages.

```bash
cs search <query> [--project <text>] [--max N]
```

### `cs stats`

Show usage statistics with fully separate sections for:

- `CLAUDE CODE`
- `CODEX`

Each section includes sessions, history entries, top models, and recent daily activity.

## TUI Keybindings

Main list view:

- `↑/↓`: move selection
- `Ctrl-U` / `Ctrl-D`: move selection up/down
- `Enter`: resume selected session
- `Option-V`: open conversation detail
- `/`: full-text search/filter sessions
- `Ctrl-C` or `q`: quit

Detail view:

- `↑/↓`: scroll
- `Esc` or `b`: back to list
- `Ctrl-C` or `q`: quit

## Resume Behavior

Resuming is source-aware:

- Claude Code: `cc --resume <id>` (fallback `claude`)
- Codex: `c resume <id>` (fallback `codex`)

Model flag is source-specific and automatically attached when known:

- Claude Code: `--model <name>`
- Codex: `-m <name>`

For Codex, when known, `cs` also restores reasoning effort with:

- `-c model_reasoning_effort="<effort>"`

If a session does not have model metadata, `cs` falls back to the most recently used model for that same source.

## Data Source Notes

- `cs` reads session histories from CLI history files (`~/.claude/history.jsonl`, `~/.codex/history.jsonl`).
- Codex Desktop conversations are not guaranteed to appear unless they are also represented in Codex CLI history/session files.
