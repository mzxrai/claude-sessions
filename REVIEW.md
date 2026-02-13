# Code Review Checklist

This checklist is based on a full read of `src/main.rs` and `README.md`. It focuses on correctness, production readiness, and maintainability for a 100+ engineer environment.

## Critical
- [x] **Codex resume may run with an invalid model string.** Fixed by validating/sanitizing model candidates before persisting and before passing `-m` in resume. (`src/main.rs`)

## High
- [x] **Codex session file parsing can still miss metadata when `session_meta` appears after the first matching `turn_context`.** Fixed by enforcing expected session checks via explicit `sessionId`/`session_id` matching and session-meta scope tracking for entries missing explicit ids. (`src/main.rs`)

## Medium
- [x] **Model label hard-coded as “Claude” in render output.** Fixed by deriving assistant label from session source (`Codex` vs `Claude`) in conversation and search rendering. (`src/main.rs`)
- [x] **Session list columns for JSON output omit `model` and `file_path`.** Fixed by including `model` and `file_path` in `--json` list output. (`src/main.rs`)
- [x] **Relative time displays “yesterday” for any 1–2 day delta, even if clocks/timezones differ.** Fixed by showing absolute date (`YYYY-MM-DD`) for entries older than 24 hours. (`src/main.rs`)

## Low
- [x] **Potential performance issue on large Codex session files.** Fixed by switching metadata extraction to streaming line reads with `BufReader`. (`src/main.rs`)
- [x] **Hard-coded widths in list view may truncate important info.** Fixed by deriving list widths from actual session data with bounded dynamic sizing. (`src/main.rs`)

## Notes
- The code now correctly handles Codex resume syntax (`codex resume <id>`) and uses the `c` alias for Codex and `cc` for Claude Code, which aligns with your request.
- The JSON parsing in `codex_file_info_from_session_file` now skips malformed lines instead of aborting the entire parse, which is the right behavior.
