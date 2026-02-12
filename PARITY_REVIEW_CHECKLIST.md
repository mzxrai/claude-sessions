# Rust `cs-rs` parity review checklist (vs Python)

## 1) CLI command parity
- [x] `view <session_id>` command exists
- [x] `view --thinking` supported
- [x] `view --tail/-t` supported
- [x] `view --no-pager` supported
- [x] `search <query>` command exists
- [x] `search --project/-p` supported
- [x] `search --max/-m` supported
- [x] `search` uses regex + case-insensitive matching
- [x] invalid regex handling is surfaced as error (Rust returns command error)
- [x] `stats` command exists
- [x] `list` command exists
- [x] `list --project/-p` supported
- [x] `list --since` supported and format validated (`YYYY-MM-DD`)
- [x] `list --limit/-n` supported
- [x] `list --json` supported
- [x] no-args mode launches interactive picker

## 2) Resume behavior parity
- [x] Enter resumes selected session
- [x] resumes through interactive `zsh` to pick up alias function (`cc` if available, else `claude`)
- [x] project dir context is chdir-ed from session metadata before resume

## 3) Session store / data behavior
- [x] history discovery from `~/.claude/history.jsonl`
- [x] transcript discovery under `~/.claude/projects`
- [x] prefix session-id matching
- [x] transcript parsing supports both JSON message object and string content
- [x] skip internal message types used by Claude (`file-history-snapshot`, `progress`, `queue-operation`)

## 4) TUI parity
- [x] list of sessions with relative age, project, title
- [x] live filtering by `display`, `project`, `session_id`
- [x] filter prompt placeholder shown
- [x] `/` enters filter mode
- [x] `s` enters filter mode (additional alias retained)
- [x] `q` quits
- [x] `Esc` exits filter mode when filtering
- [x] `v` opens in-place conversation detail
- [x] `Enter` resumes
- [x] `↑/↓` navigate
- [x] detail mode supports `Esc/b` back and `q` quit
- [x] bottom key menu/status text is shown in both modes
- [x] filter mode no longer steals one-off control key intents (`Ctrl-C`, `Ctrl-D`)

## 5) Output formatting parity
- [x] `list --json` includes `session_id/display/project/timestamp`
- [x] `stats` includes overview, model usage, daily activity, hour activity
- [x] make output formatting and spacing exactly match Python Rich rendering
  - [x] footer-like tiny key legend style (visual polish)
  - [x] exact bar widths/labels for activity sections
  - [x] exact ordering / spacing in model usage tables

## 6) Build health
- [x] `cargo check` succeeds in repo root
