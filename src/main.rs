use anyhow::{anyhow, Context, Result};
use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;

const INTERNAL_TYPES: [&str; 3] = ["file-history-snapshot", "progress", "queue-operation"];

#[derive(Clone, Debug, Serialize)]
struct SessionInfo {
    session_id: String,
    display: String,
    project: String,
    timestamp: i64,
    file_path: Option<String>,
}

impl SessionInfo {
    fn short_id(&self) -> &str {
        &self.session_id[..self.session_id.len().min(8)]
    }

    
}

#[derive(Deserialize)]
struct HistoryEntry {
    #[serde(alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    display: String,
    #[serde(default)]
    timestamp: i64,
    #[serde(default)]
    project: String,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    timestamp: String,
    #[serde(rename = "isApiErrorMessage", default)]
    is_api_error: bool,
    #[serde(rename = "sessionId", default)]
    session_id: String,
    #[serde(default)]
    message: Value,
}

#[derive(Clone)]
struct Message {
    msg_type: String,
    _uuid: String,
    _timestamp: String,
    is_api_error: bool,
    _session_id: String,
    message: Value,
}

impl From<RawMessage> for Message {
    fn from(raw: RawMessage) -> Self {
        Self {
            msg_type: raw.msg_type.unwrap_or_default(),
            _uuid: raw.uuid,
            _timestamp: raw.timestamp,
            is_api_error: raw.is_api_error,
            _session_id: raw.session_id,
            message: raw.message,
        }
    }
}

impl Message {
    fn role(&self) -> &str {
        self.message
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| Some(self.msg_type.as_str()))
            .unwrap_or("assistant")
    }

    fn model(&self) -> &str {
        self.message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn content_blocks(&self) -> Vec<Value> {
        match self.message.get("content") {
            Some(Value::String(s)) => vec![json!({
                "type": "text",
                "text": s,
            })],
            Some(Value::Array(blocks)) => blocks.clone(),
            _ => Vec::new(),
        }
    }

    fn text(&self) -> String {
        self.content_blocks()
            .into_iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block_text(&block)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

}

fn block_text(block: &Value) -> Option<String> {
    block.get("text").and_then(Value::as_str).map(str::to_string)
}

fn format_with_commas(n: u64) -> String {
    let mut output = String::new();
    for (i, ch) in n.to_string().chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    output.chars().rev().collect()
}

fn human_file_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if size == 0 {
        return "0 B".to_string();
    }

    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

fn file_size_for_session(file_path: &Option<String>) -> (String, bool) {
    match file_path.as_deref().and_then(|path| fs::metadata(path).ok()).map(|m| m.len()) {
        Some(size) => (human_file_size(size), size > 1_048_576),
        None => ("—".to_string(), false),
    }
}

struct SessionStore {
    sessions: HashMap<String, SessionInfo>,
    loaded: bool,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            loaded: false,
        }
    }

    fn history_file() -> PathBuf {
        home_dir().join(".claude/history.jsonl")
    }

    fn projects_dir() -> PathBuf {
        home_dir().join(".claude/projects")
    }

    fn stats_file() -> PathBuf {
        home_dir().join(".claude/stats-cache.json")
    }

    fn encode_path(path: &str) -> String {
        path.replace('/', "-")
    }

    fn load(&mut self) {
        if self.loaded {
            return;
        }

        let history_path = Self::history_file();
        if !history_path.exists() {
            self.loaded = true;
            return;
        }

        let mut seen: HashMap<String, SessionInfo> = HashMap::new();
        let content = fs::read_to_string(&history_path).unwrap_or_default();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let entry: HistoryEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let session_id = match entry.session_id {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };

            match seen.get_mut(&session_id) {
                Some(existing) => {
                    if entry.timestamp > existing.timestamp {
                        existing.timestamp = entry.timestamp;
                    }
                }
                None => {
                    let file_path = self.find_session_file(&session_id, &entry.project);
                    seen.insert(
                        session_id.clone(),
                        SessionInfo {
                            session_id,
                            display: entry.display,
                            project: entry.project,
                            timestamp: entry.timestamp,
                            file_path: file_path.map(|p| p.to_string_lossy().to_string()),
                        },
                    );
                }
            }
        }

        self.sessions = seen;
        self.loaded = true;
    }

    fn all(&mut self) -> Vec<SessionInfo> {
        self.load();
        let mut out: Vec<_> = self.sessions.values().cloned().collect();
        out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        out
    }

    fn get(&mut self, session_id: &str) -> Option<SessionInfo> {
        self.load();
        if let Some(session) = self.sessions.get(session_id) {
            return Some(session.clone());
        }

        let mut matches = Vec::new();
        for (id, session) in &self.sessions {
            if id.starts_with(session_id) {
                matches.push(session.clone());
            }
        }
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    fn find_session_file(&self, session_id: &str, project: &str) -> Option<PathBuf> {
        if !project.is_empty() {
            let encoded = Self::encode_path(project);
            let candidate = Self::projects_dir().join(encoded).join(format!("{session_id}.jsonl"));
            if candidate.exists() {
                return Some(candidate);
            }
        }

        let projects_dir = Self::projects_dir();
        if !projects_dir.exists() {
            return None;
        }

        let readdir = fs::read_dir(projects_dir).ok()?;
        for entry in readdir.filter_map(Result::ok) {
            let p = entry.path();
            if p.is_dir() {
                let cand = p.join(format!("{session_id}.jsonl"));
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
        None
    }

    fn read_messages(&self, session: &SessionInfo, skip_internal: bool) -> Vec<Message> {
        let path = match session.file_path.as_deref() {
            Some(p) => p,
            None => return Vec::new(),
        };
        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let raw: RawMessage = match serde_json::from_str(line) {
                Ok(raw) => raw,
                Err(_) => continue,
            };

            let msg: Message = raw.into();
            if skip_internal && INTERNAL_TYPES.contains(&msg.msg_type.as_str()) {
                continue;
            }
            out.push(msg);
        }
        out
    }

    fn search(
        &mut self,
        query: &str,
        project: Option<&str>,
        max_results: usize,
    ) -> Result<Vec<(SessionInfo, Message, String)>> {
        self.load();

        let pattern = Regex::new(&format!("(?i){query}"))
            .map_err(|err| anyhow!("invalid regex: {err}"))?;

        let mut results: Vec<(SessionInfo, Message, String)> = Vec::new();
        for session in self.all() {
            if let Some(p) = project {
                if !session.project.to_lowercase().contains(&p.to_lowercase()) {
                    continue;
                }
            }

            for msg in self.read_messages(&session, true) {
                let text = msg.text();
                if text.is_empty() {
                    continue;
                }
                let found = text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .find(|line| pattern.is_match(line));

                if let Some(line) = found {
                    results.push((session.clone(), msg.clone(), line.to_string()));
                    if results.len() >= max_results {
                        return Ok(results);
                    }
                    break;
                }
            }
        }

        Ok(results)
    }

    fn load_stats(&self) -> Option<Value> {
        let path = Self::stats_file();
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

fn relative_time(ts_ms: i64) -> String {
    let now = Local::now();
    let when = match Local.timestamp_millis_opt(ts_ms).single() {
        Some(ts) => ts,
        None => return "—".to_string(),
    };
    let delta = now.signed_duration_since(when);
    let mins = delta.num_minutes();
    let hrs = delta.num_hours();
    let days = delta.num_days();
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if hrs < 24 {
        format!("{hrs}h ago")
    } else if days < 2 {
        "yesterday".to_string()
    } else if days < 7 {
        format!("{days} days ago")
    } else if days < 30 {
        format!("{weeks}w ago")
    } else if days < 365 {
        format!("{months}mo ago")
    } else {
        format!("{years}y ago")
    }
}

fn short_project(project: &str) -> String {
    let home = home_dir();
    let home_s = home.to_string_lossy();
    if let Some(rest) = project.strip_prefix(home_s.as_ref()) {
        format!("~{rest}")
    } else {
        project.to_string()
    }
}

fn truncate(text: &str, width: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = text.chars().count();
    if count <= width {
        text
    } else {
        text.chars()
            .take(width.saturating_sub(3))
            .collect::<String>()
            + "..."
    }
}

fn render_conversation(store: &SessionStore, session: &SessionInfo, thinking: bool, tail: Option<usize>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("Session: {}", truncate(&session.display, 120)));
    lines.push(format!(
        "{}  ·  {}  ·  {}",
        short_project(&session.project),
        relative_time(session.timestamp),
        session.short_id()
    ));
    lines.push(String::new());

    let mut msgs = store.read_messages(session, true);
    if let Some(t) = tail {
        let start = msgs.len().saturating_sub(t);
        msgs = msgs.into_iter().skip(start).collect();
    }

    for msg in msgs {
        if msg.msg_type == "system" {
            continue;
        }

        if msg.msg_type == "user" {
            let text = msg.text();
            if text.is_empty() {
                continue;
            }
            if text.starts_with("<local-command") || text.starts_with("<command-name") {
                continue;
            }
            lines.push(format!("You: {text}"));
            lines.push(String::new());
            continue;
        }

        if msg.msg_type == "assistant" {
            if msg.is_api_error {
                lines.push(format!("Error: {}", truncate(&msg.text(), 500)));
                lines.push(String::new());
                continue;
            }

            let mut parts: Vec<String> = Vec::new();
            for block in msg.content_blocks() {
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                if btype == "text" {
                    let text = block_text(&block).unwrap_or_default();
                    if !text.trim().is_empty() {
                        parts.push(text);
                    }
                } else if btype == "tool_use" {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let input = block.get("input").unwrap_or(&Value::Null);
                    let summary = match name {
                        "Bash" => {
                            let cmd = input
                                .get("command")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let desc = input
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let detail = if desc.is_empty() { cmd } else { desc };
                            format!("$ {}", truncate(detail, 80))
                        }
                        "Read" | "Edit" | "Write" | "Glob" | "Grep" => {
                            let target = input
                                .get("file_path")
                                .or_else(|| input.get("pattern"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            format!("{name} {target}")
                        }
                        "Task" => {
                            let desc = input.get("description").and_then(Value::as_str).unwrap_or("");
                            format!("Task {desc}")
                        }
                        "WebSearch" => {
                            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
                            format!("Search: {query}")
                        }
                        _ => format!("{name}(...)"),
                    };
                    parts.push(format!("[tool] {summary}"));
                } else if btype == "thinking" {
                    if thinking {
                        let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !thinking.trim().is_empty() {
                            parts.push(format!("[thinking] {}", truncate(thinking, 250)));
                        }
                    }
                }
            }

            if !parts.is_empty() {
                let model = msg.model();
                if model.is_empty() {
                    lines.push(format!("Claude: {}", parts.join("\n")));
                } else if model != "<synthetic>" {
                    lines.push(format!("Claude ({model}): {}", parts.join("\n")));
                } else {
                    lines.push(format!("Claude: {}", parts.join("\n")));
                }
                lines.push(String::new());
            }
        }
    }

    lines
}

fn render_search_results(results: Vec<(SessionInfo, Message, String)>) -> String {
    if results.is_empty() {
        return "No matches found.\n".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!("{} match(es)\n\n", results.len()));
    for (session, msg, line) in results {
        out.push_str(&format!(
            "{}  {}  {}\n",
            session.short_id(),
            relative_time(session.timestamp),
            short_project(&session.project)
        ));
        out.push_str(&format!("  {}\n", truncate(&session.display, 80)));
        let role = msg.role();
        let role_color = if role == "user" { "You" } else { "Claude" };
        out.push_str(&format!("  {role_color}: {}\n\n", truncate(&line, 100)));
    }
    out
}

fn render_stats(stats: &Value) -> String {
    fn short_model(name: &str) -> String {
        let trimmed = name.strip_prefix("claude-").unwrap_or(name);
        trimmed
            .split("-202")
            .next()
            .unwrap_or(trimmed)
            .to_string()
    }

    fn render_bar(count: u64, max_count: u64, width: usize) -> String {
        if max_count == 0 || width == 0 {
            return String::new();
        }

        let n = ((count.saturating_mul(width as u64)) / max_count) as usize;
        "█".repeat(n.min(width))
    }

    fn table_divider(left: &str, mid: &str, right: &str, columns: &[usize]) -> String {
        let mut out = String::new();
        out.push_str(left);
        for (i, width) in columns.iter().enumerate() {
            out.push_str(&"━".repeat(width + 2));
            if i + 1 < columns.len() {
                out.push_str(mid);
            }
        }
        out.push_str(right);
        out
    }

    let mut out = String::new();
    const FRAME_W: usize = 82;
    let title = "Claude Code Usage Stats";
    out.push_str(&format!("╭{}╮\n", "─".repeat(FRAME_W - 2)));
    out.push_str(&format!("│{:^width$}│\n", title, width = FRAME_W - 2));
    out.push_str(&format!("╰{}╯\n\n", "─".repeat(FRAME_W - 2)));

    let total_sessions = stats.get("totalSessions").and_then(Value::as_u64).unwrap_or(0);
    let total_messages = stats.get("totalMessages").and_then(Value::as_u64).unwrap_or(0);
    let first = stats
        .get("firstSessionDate")
        .and_then(Value::as_str)
        .unwrap_or("—");
    let last = stats
        .get("lastComputedDate")
        .and_then(Value::as_str)
        .unwrap_or("—");
    let first_short = if first.len() >= 10 { &first[..10] } else { first };

    out.push_str(&format!("Total sessions: {total_sessions}\n"));
    out.push_str(&format!("Total messages: {total_messages}\n"));
    out.push_str(&format!("First session: {first_short}\n"));
    out.push_str(&format!("Last computed: {last}\n"));
    out.push('\n');

    if let Some(longest) = stats.get("longestSession").and_then(Value::as_object) {
        let dur = longest.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        let msgs = longest.get("messageCount").and_then(Value::as_u64).unwrap_or(0);
        out.push_str(&format!(
            "Longest session: {:.1}h ({} msgs)\n",
            dur / 1000.0 / 3600.0,
            msgs
        ));
        out.push('\n');
    }

    if let Some(model_usage) = stats.get("modelUsage").and_then(Value::as_object) {
        let mut rows: Vec<(String, u64, u64, u64)> = Vec::new();
        for (name, v) in model_usage {
            let output_tokens = v.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
            let cache_read = v
                .get("cacheReadInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = v
                .get("cacheCreationInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            rows.push((short_model(name), output_tokens, cache_read, cache_write));
        }

        rows.sort_by(|a, b| b.1.cmp(&a.1));

        let model_width = rows
            .iter()
            .map(|r| r.0.len())
            .max()
            .unwrap_or(5)
            .max(5);
        let out_width = rows
            .iter()
            .map(|r| format_with_commas(r.1).len())
            .max()
            .unwrap_or(1)
            .max(13);
        let cache_r_width = rows
            .iter()
            .map(|r| format_with_commas(r.2).len())
            .max()
            .unwrap_or(1)
            .max(10);
        let cache_w_width = rows
            .iter()
            .map(|r| format_with_commas(r.3).len())
            .max()
            .unwrap_or(1)
            .max(11);
        let columns = [model_width, out_width, cache_r_width, cache_w_width];

        out.push_str("Model Usage:\n");
        out.push_str(&table_divider("┏", "┳", "┓", &columns));
        out.push('\n');
        out.push_str(&format!(
            "┃ {model:<mw$} ┃ {out:>ow$} ┃ {cr:>crw$} ┃ {cw:>cww$} ┃\n",
            model = "Model",
            out = "Output",
            cr = "Cache read",
            cw = "Cache write",
            mw = model_width,
            ow = out_width,
            crw = cache_r_width,
            cww = cache_w_width
        ));
        out.push_str(&table_divider("┣", "╋", "┫", &columns));
        out.push('\n');
        for (model, output_tokens, cache_read, cache_write) in rows {
            out.push_str(&format!(
                "┃ {model:<mw$} ┃ {out:>ow$} ┃ {cr:>crw$} ┃ {cw:>cww$} ┃\n",
                model = model,
                out = format_with_commas(output_tokens),
                cr = format_with_commas(cache_read),
                cw = format_with_commas(cache_write),
                mw = model_width,
                ow = out_width,
                crw = cache_r_width,
                cww = cache_w_width
            ));
        }
        out.push_str(&table_divider("┗", "┻", "┛", &columns));
        out.push('\n');
    }

    if let Some(daily) = stats.get("dailyActivity").and_then(Value::as_array) {
        if !daily.is_empty() {
            let date_w = 10usize;
            let sessions_w = 8usize;
            let messages_w = 9usize;
            let tools_w = 10usize;
            let bar_w = 20usize;
            let columns = [date_w, sessions_w, messages_w, tools_w, bar_w];

            out.push_str("Daily Activity:\n");
            out.push_str(&table_divider("┏", "┳", "┓", &columns));
            out.push('\n');
            out.push_str(&format!(
                "┃ {date:<dw$} ┃ {sessions:>sw$} ┃ {messages:>mw$} ┃ {tools:>tw$} ┃ {activity:<bw$} ┃\n",
                date = "Date",
                sessions = "Sessions",
                messages = "Messages",
                tools = "Tool calls",
                activity = "Activity",
                dw = date_w,
                sw = sessions_w,
                mw = messages_w,
                tw = tools_w,
                bw = bar_w
            ));
            out.push_str(&table_divider("┣", "╋", "┫", &columns));
            out.push('\n');

            let start = if daily.len() > 14 { daily.len() - 14 } else { 0 };
            let window = &daily[start..];
            let max_msgs = window
                .iter()
                .map(|d| d.get("messageCount").and_then(Value::as_u64).unwrap_or(0))
                .max()
                .unwrap_or(1);

            for day in window {
                let date = day.get("date").and_then(Value::as_str).unwrap_or("");
                let sessions = day.get("sessionCount").and_then(Value::as_u64).unwrap_or(0);
                let msgs = day.get("messageCount").and_then(Value::as_u64).unwrap_or(0);
                let tools = day.get("toolCallCount").and_then(Value::as_u64).unwrap_or(0);
                let bar = render_bar(msgs, max_msgs, bar_w);
                out.push_str(&format!(
                    "┃ {date:<dw$} ┃ {sessions:>sw$} ┃ {msgs:>mw$} ┃ {tools:>tw$} ┃ {bar:<bw$} ┃\n",
                    date = date,
                    sessions = sessions,
                    msgs = msgs,
                    tools = tools,
                    bar = bar,
                    dw = date_w,
                    sw = sessions_w,
                    mw = messages_w,
                    tw = tools_w,
                    bw = bar_w
                ));
            }
            out.push_str(&table_divider("┗", "┻", "┛", &columns));
            out.push('\n');
        }
    }

    if let Some(hours) = stats.get("hourCounts").and_then(Value::as_object) {
        if !hours.is_empty() {
            out.push_str("Activity by hour:\n");
            let max_count = hours.values().filter_map(Value::as_u64).max().unwrap_or(1);
            for hour in 0..24 {
                let count = hours.get(&hour.to_string()).and_then(Value::as_u64).unwrap_or(0);
                let bar = render_bar(count, max_count, 30);
                if bar.is_empty() {
                    out.push_str(&format!("  {:02}:00          (0)\n", hour));
                } else {
                    out.push_str(&format!("  {:02}:00 {:>9} {bar}\n", hour, count));
                }
            }
            out.push('\n');
        }
    }

    out
}

fn list_sessions(sessions: Vec<SessionInfo>, json_output: bool, max_count: usize) -> String {
    let mut out = String::new();
    let subset: Vec<_> = sessions.into_iter().take(max_count).collect();

    if json_output {
        let data: Vec<_> = subset
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "session_id": s.session_id,
                    "display": s.display,
                    "project": s.project,
                    "timestamp": s.timestamp,
                })
            })
            .collect();
        let value = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "[]".to_string());
        return value;
    }

    out.push_str("  ID    time     project                    title\n");
    out.push_str(&"-".repeat(72));
    out.push('\n');
    for s in subset {
        let short_id = s.short_id();
        let time = relative_time(s.timestamp);
        let proj = truncate(&short_project(&s.project), 24).chars().take(24).collect::<String>();
        let title = truncate(&s.display, 36);
        out.push_str(&format!("{short_id:8}  {time:<8}  {proj:<24}  {title}\n"));
    }
    out
}

fn run_tui() -> Result<()> {
    let mut store = SessionStore::new();
    let sessions = store.all();
    let mut filtered = sessions.clone();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let mut filter = String::new();
    let mut filter_input = false;
    let mut in_detail = false;
    let mut detail_lines = Vec::<String>::new();
    let mut detail_scroll: usize = 0;

    let mut terminal = init_terminal()?;

    loop {
        terminal.draw(|f| {
            let size = f.size();
            let top_height = if in_detail { 0u16 } else { (filter_input as u16) + 1 };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(top_height),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(size);

            let status = if in_detail {
                " [↑/↓] scroll  [Esc]/[b] back  [q] quit"
            } else {
                " [↑/↓] navigate  [Enter] resume  [v] view  [/] search  [q] quit"
            };

            if !in_detail {
                let filter_text = if filter_input {
                    format!("> {filter}")
                } else {
                    "Type to search sessions...".to_string()
                };
                f.render_widget(
                    Paragraph::new(filter_text)
                        .style(Style::default().fg(Color::White))
                        .block(Block::default().borders(Borders::BOTTOM)),
                    chunks[0],
                );
            }

            if in_detail {
                let line_count = detail_lines.len();
                let visible = chunks[1].height as usize;
                let end = detail_lines.len().min(detail_scroll + visible);
                let slice = if detail_scroll < line_count {
                    detail_lines[detail_scroll..end].to_vec()
                } else {
                    Vec::new()
                };
                let lines: Vec<Line> = slice
                    .iter()
                    .map(|line| Line::from(line.clone()))
                    .collect();
                f.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title("Session")),
                    chunks[1],
                );
            } else {
                let items: Vec<ListItem> = filtered
                    .iter()
                    .map(|s| {
                        let time = relative_time(s.timestamp);
                        let project = truncate(&short_project(&s.project), 24);
                        let (size, is_large_size) = file_size_for_session(&s.file_path);
                        let prompt_w =
                            (chunks[1].width as usize)
                                .saturating_sub(10 + 3 + 24 + 3 + 8 + 3)
                                .max(20);
                        let prompt = truncate(&s.display, prompt_w);
                        let size_style = if is_large_size {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        let row = Line::from(vec![
                            Span::styled(format!("{time:>10}"), Style::default().fg(Color::DarkGray)),
                            Span::from("   "),
                            Span::styled(format!("{project:24}"), Style::default().fg(Color::Cyan)),
                            Span::from("   "),
                            Span::styled(format!("{size:>8}"), size_style),
                            Span::from("   "),
                            Span::from(prompt),
                        ]);
                        ListItem::new(row)
                    })
                    .collect();

                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title("Sessions"))
                    .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                    .highlight_symbol("> ");
                f.render_stateful_widget(list, chunks[1], &mut list_state);
            }

            f.render_widget(
                Paragraph::new(status)
                    .style(Style::default().fg(Color::White)),
                chunks[2],
            );
        })?;

        if !event::poll(StdDuration::from_millis(200))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        if in_detail {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc | KeyCode::Char('b') => {
                    in_detail = false;
                    detail_scroll = 0;
                    detail_lines.clear();
                }
                KeyCode::Up => {
                    detail_scroll = detail_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    if detail_scroll + 1 < detail_lines.len() {
                        detail_scroll += 1;
                    }
                }
                _ => {}
            }
            continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => break,
                _ => {}
            }
        }

        if filter_input {
            match key.code {
                KeyCode::Esc => {
                    filter_input = false;
                    filter.clear();
                    apply_filter(&mut filtered, &sessions, &filter);
                    list_state.select(Some(0));
                }
                KeyCode::Backspace => {
                    filter.pop();
                    apply_filter(&mut filtered, &sessions, &filter);
                    list_state.select(Some(0));
                }
                KeyCode::Char(c) => {
                    if !c.is_control() {
                        filter.push(c);
                    }
                    apply_filter(&mut filtered, &sessions, &filter);
                    list_state.select(Some(0));
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('/') => {
                filter_input = true;
                filter.clear();
            }
            KeyCode::Esc => break,
            KeyCode::Up => {
                let prev = match list_state.selected() {
                    Some(0) | None => 0,
                    Some(i) => i.saturating_sub(1),
                };
                list_state.select(Some(prev));
            }
            KeyCode::Down => {
                let len = filtered.len();
                let next = match list_state.selected() {
                    Some(i) => {
                        if i + 1 < len {
                            i + 1
                        } else {
                            i
                        }
                    }
                    None => if len > 0 { 0 } else { 0 },
                };
                list_state.select(Some(next));
            }
            KeyCode::Enter => {
                let idx = list_state.selected().unwrap_or_default();
                if idx < filtered.len() {
                    let session = filtered[idx].clone();
                    cleanup_terminal(&mut terminal)?;
                    resume_session(&session)?;
                    return Ok(());
                }
            }
            KeyCode::Char('v') => {
                let idx = list_state.selected().unwrap_or(0);
                if idx < filtered.len() {
                    let session = &filtered[idx];
                    detail_lines = render_conversation(&store, session, false, None);
                    in_detail = true;
                    detail_scroll = 0;
                }
            }
            _ => {}
        }
    }

    cleanup_terminal(&mut terminal)?;
    Ok(())
}

fn apply_filter(filtered: &mut Vec<SessionInfo>, sessions: &[SessionInfo], filter: &str) {
    let q = filter.to_lowercase();
    if q.is_empty() {
        filtered.clear();
        filtered.extend_from_slice(sessions);
        return;
    }

    filtered.clear();
    for session in sessions {
        if session.display.to_lowercase().contains(&q)
            || session.project.to_lowercase().contains(&q)
            || session.session_id.to_lowercase().contains(&q)
        {
            filtered.push(session.clone());
        }
    }
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn cleanup_terminal<B: std::io::Write>(terminal: &mut Terminal<CrosstermBackend<B>>) -> Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    let escaped = value.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn resume_session(session: &SessionInfo) -> Result<()> {
    if let Some(project_path) = Path::new(&session.project).canonicalize().ok() {
        let _ = std::env::set_current_dir(project_path);
    }

    let session_id = shell_single_quote(&session.session_id);
    let script = format!(
        "cc_session_id={}; if whence -w cc >/dev/null 2>&1; then cc --resume \"$cc_session_id\"; else claude --resume \"$cc_session_id\"; fi",
        session_id
    );

    let mut cmd = Command::new("zsh");
    cmd.arg("-ic").arg(script);
    let status = cmd
        .status()
        .with_context(|| "failed to launch shell resume command")?;

    if !status.success() {
        eprintln!("Resume command exited with status: {status}");
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| String::from("/Users/mbm-gsc")))
}

fn list_command(store: &mut SessionStore, project: Option<String>, since: Option<String>, limit: usize, json: bool) -> Result<String> {
    let mut sessions = store.all();

    if let Some(p) = project.as_deref() {
        let p = p.to_lowercase();
        sessions = sessions
            .into_iter()
            .filter(|s| s.project.to_lowercase().contains(&p))
            .collect();
    }

    if let Some(since_s) = since {
        let since_ms = chrono::NaiveDate::parse_from_str(&since_s, "%Y-%m-%d")
            .map(|date| {
                date
                    .and_hms_opt(0, 0, 0)
                    .and_then(|naive| Local.from_local_datetime(&naive).single())
                    .map(|ts| ts.timestamp_millis())
            })
            .ok()
            .flatten()
            .with_context(|| format!("Invalid date format: {since_s} (use YYYY-MM-DD)"))?;

        sessions = sessions
            .into_iter()
            .filter(|s| s.timestamp >= since_ms)
            .collect();
        }

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(list_sessions(sessions, json, limit))
}

#[derive(Parser)]
#[command(name = "cs-rs", about = "Claude Sessions in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    View {
        session_id: String,
        #[arg(long)]
        thinking: bool,
        #[arg(short, long)]
        tail: Option<usize>,
        #[arg(long)]
        no_pager: bool,
    },
    Search {
        query: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long, default_value_t = 50)]
        max: usize,
    },
    Stats,
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long)]
        since: Option<String>,
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut store = SessionStore::new();

    match cli.command {
        None => {
            run_tui()?;
        }
        Some(Commands::View {
            session_id,
            thinking,
            tail,
            no_pager,
        }) => {
            let session = store
                .get(&session_id)
                .with_context(|| format!("Session not found: {session_id}"))?;
            let lines = render_conversation(&store, &session, thinking, tail);
            output_with_optional_pager(&lines.join("\n"), no_pager)?;
        }
        Some(Commands::Search { query, project, max }) => {
            let results = store.search(&query, project.as_deref(), max)?;
            println!("{}", render_search_results(results));
        }
        Some(Commands::Stats) => {
            let stats = store
                .load_stats()
                .context("No stats-cache.json found")?;
            println!("{}", render_stats(&stats));
        }
        Some(Commands::List { project, since, limit, json }) => {
            let output = list_command(&mut store, project, since, limit, json)?;
            println!("{}", output);
        }
    }

    Ok(())
}

fn output_with_optional_pager(output: &str, no_pager: bool) -> Result<()> {
    if no_pager || !io::stdout().is_terminal() {
        println!("{output}");
        return Ok(());
    }

    let mut proc = match Command::new("less")
        .arg("-R")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .spawn()
    {
        Ok(proc) => proc,
        Err(_) => {
            println!("{output}");
            return Ok(());
        }
    };

    if let Some(mut stdin) = proc.stdin.take() {
        if let Err(err) = stdin.write_all(output.as_bytes()) {
            let _ = err;
            println!("{output}");
            return Ok(());
        }
    }
    proc.wait().with_context(|| "pager exited with error")?;

    Ok(())
}
