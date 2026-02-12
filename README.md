# Claude Sessions (Rust)

This is the Rust/TUI implementation of `cs`, a command-line session browser for Claude Code sessions.

## Overview

- Lists and filters recent sessions from `~/.claude/history.jsonl`
- Shows session metadata (time, project, size)
- Lets you view full conversation logs in an interactive terminal UI
- Supports searching sessions
- Supports resume (`cc`) workflow and session detail views

## Build

```bash
cargo build --release
```

The release binary is produced at `target/release/cs-rs`.

## Run

```bash
cs [command]
```

Common commands:

- `cs list` — open the interactive session list
- `cs view <session-id>` — view one session
- `cs search <query>` — search session text/messages
- `cs stats` — show basic session stats

## Installation stub

The launcher script at `~/.local/bin/cs` should point to this project's release binary:

```bash
exec /Users/mbm-gsc/permanent/claude-sessions/target/release/cs-rs "$@"
```
