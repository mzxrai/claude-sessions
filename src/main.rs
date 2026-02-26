use anyhow::{anyhow, Context, Result};
use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File};
use std::io::{self, stdout, BufRead, BufReader, IsTerminal, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;
use std::time::UNIX_EPOCH;

const INTERNAL_TYPES: [&str; 3] = ["file-history-snapshot", "progress", "queue-operation"];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
enum SessionSource {
    Claudecode,
    Codex,
}

impl SessionSource {
    fn all() -> &'static [Self] {
        &[Self::Claudecode, Self::Codex]
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Claudecode => "claude code",
            Self::Codex => "codex",
        }
    }

    fn list_label(&self) -> &'static str {
        match self {
            Self::Claudecode => "cc",
            Self::Codex => "codex",
        }
    }

    fn cache_key(&self) -> &'static str {
        match self {
            Self::Claudecode => "claudecode",
            Self::Codex => "codex",
        }
    }

    fn resume_command(&self) -> &'static str {
        match self {
            Self::Claudecode => "cc",
            Self::Codex => "c",
        }
    }

    fn fallback_resume_command(&self) -> &'static str {
        match self {
            Self::Claudecode => "claude",
            Self::Codex => "codex",
        }
    }

    fn resume_invocation(&self) -> &'static str {
        match self {
            Self::Claudecode => "--resume \"$cs_session_id\"",
            Self::Codex => "resume \"$cs_session_id\"",
        }
    }

    fn resume_model_flag(&self) -> &'static str {
        match self {
            Self::Claudecode => "--model",
            Self::Codex => "-m",
        }
    }

    fn history_file(&self) -> PathBuf {
        self.home_base().join("history.jsonl")
    }

    fn projects_dir(&self) -> PathBuf {
        self.home_base().join("projects")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.home_base().join("sessions")
    }

    fn archived_sessions_dir(&self) -> PathBuf {
        self.home_base().join("archived_sessions")
    }

    fn home_base(&self) -> PathBuf {
        match self {
            Self::Claudecode => home_dir().join(".claude"),
            Self::Codex => home_dir().join(".codex"),
        }
    }

    fn internal_key(&self, session_id: &str) -> String {
        format!("{}::{session_id}", self.label())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionInfo {
    source: SessionSource,
    session_id: String,
    display: String,
    project: String,
    timestamp: i64,
    model: String,
    #[serde(default)]
    reasoning_effort: String,
    file_path: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct SessionCache {
    version: u32,
    histories: HashMap<String, CachedHistory>,
    codex_sessions: HashMap<String, CachedCodexSession>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CachedHistory {
    file_size: u64,
    file_modified_ms: i64,
    #[serde(default)]
    line_count: u64,
    sessions: Vec<SessionInfo>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CachedCodexSession {
    file_path: String,
    file_size: u64,
    file_modified_ms: i64,
    cwd: Option<String>,
    timestamp_ms: Option<i64>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Clone)]
struct SearchTextCacheEntry {
    file_size: u64,
    file_modified_ms: i64,
    text: String,
}

struct StatsSourceRow {
    source: SessionSource,
    sessions: u64,
    history_entries: u64,
    first_session_date: String,
    top_models: Vec<(String, u64)>,
    daily_sessions: Vec<(String, u64)>,
}

struct StatsReport {
    total_sessions: u64,
    total_history_entries: u64,
    last_computed_date: String,
    sources: Vec<StatsSourceRow>,
}

impl SessionInfo {
    fn short_id(&self) -> &str {
        &self.session_id[..self.session_id.len().min(8)]
    }

    fn list_id_tail(&self) -> String {
        session_id_hex_tail(&self.session_id, 5)
    }
}

#[derive(Deserialize)]
struct HistoryEntry {
    #[serde(alias = "sessionId")]
    session_id: Option<String>,
    session_id_legacy: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(alias = "ts", default)]
    ts: Option<i64>,
    #[serde(default)]
    display: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    project: String,
}

#[derive(Deserialize)]
struct CodexSessionMeta {
    #[serde(default)]
    id: String,
    #[serde(default)]
    timestamp: String,
}

#[derive(Default)]
struct CodexSessionFileInfo {
    cwd: Option<String>,
    timestamp_ms: Option<i64>,
    model: Option<String>,
    reasoning_effort: Option<String>,
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
            .or(Some(self.msg_type.as_str()))
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
            .filter_map(|block| block_text(&block))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }
}

fn parse_codex_message(line: &str) -> Option<Message> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }

    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }

    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let msg_type = match role {
        "user" => "user",
        "assistant" => "assistant",
        "developer" => "assistant",
        _ => "assistant",
    };

    Some(Message {
        msg_type: msg_type.to_string(),
        _uuid: String::new(),
        _timestamp: value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        is_api_error: false,
        _session_id: payload
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| value.get("sessionId").and_then(Value::as_str))
            .or_else(|| payload.get("session_id").and_then(Value::as_str))
            .or_else(|| value.get("session_id").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        message: json!({
            "role": role,
            "content": payload.get("content").unwrap_or(&Value::Null),
            "model": payload.get("model").unwrap_or(&Value::Null),
        }),
    })
}

fn block_text(block: &Value) -> Option<String> {
    if !matches!(
        block.get("type").and_then(Value::as_str),
        Some("text") | Some("input_text") | Some("output_text")
    ) {
        return None;
    }

    block
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn normalize_timestamp(ts: Option<i64>) -> i64 {
    match ts {
        Some(raw) if raw > 0 && raw < 1_000_000_000_000 => raw * 1000,
        Some(raw) => raw,
        None => 0,
    }
}

fn codex_iso_to_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn codex_model_candidate(model: &str) -> Option<String> {
    let first = model
        .split_whitespace()
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if first.is_empty() || first == "<synthetic>" {
        return None;
    }

    let is_valid = first
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/'));
    if is_valid {
        Some(first.to_string())
    } else {
        None
    }
}

fn codex_effort_candidate(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = trimmed.to_ascii_lowercase();
    let is_valid = normalized
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
    if is_valid {
        Some(normalized)
    } else {
        None
    }
}

fn codex_entry_session_id(value: &Value) -> Option<&str> {
    value
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_id").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("payload")
                .and_then(|payload| payload.get("sessionId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("payload")
                .and_then(|payload| payload.get("session_id"))
                .and_then(Value::as_str)
        })
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
    match file_path
        .as_deref()
        .and_then(|path| fs::metadata(path).ok())
        .map(|m| m.len())
    {
        Some(size) => (human_file_size(size), size > 1_048_576),
        None => ("—".to_string(), false),
    }
}

fn session_mtime_ms(file_path: &Option<String>) -> Option<i64> {
    file_path
        .as_deref()
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|metadata| SessionStore::metadata_modified_ms(&metadata))
}

fn list_time_ms_for_session(session: &SessionInfo) -> i64 {
    session_mtime_ms(&session.file_path).unwrap_or(session.timestamp)
}

fn list_time(session_ts_ms: i64) -> String {
    let now = Local::now();
    let when = match Local.timestamp_millis_opt(session_ts_ms).single() {
        Some(ts) => ts,
        None => return "—".to_string(),
    };
    let delta = now.signed_duration_since(when);
    let mins = delta.num_minutes();
    let hrs = delta.num_hours();

    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if hrs < 24 {
        format!("{hrs}h ago")
    } else {
        when.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn build_list_time_map(sessions: &[SessionInfo]) -> HashMap<String, i64> {
    sessions
        .iter()
        .map(|session| {
            (
                session.source.internal_key(&session.session_id),
                list_time_ms_for_session(session),
            )
        })
        .collect()
}

struct SessionStore {
    sessions: HashMap<String, SessionInfo>,
    loaded: bool,
    cache: SessionCache,
    cache_dirty: bool,
    search_text_cache: HashMap<String, SearchTextCacheEntry>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            loaded: false,
            cache: Self::load_cache(),
            cache_dirty: false,
            search_text_cache: HashMap::new(),
        }
    }

    fn encode_path(path: &str) -> String {
        path.replace('/', "-")
    }

    fn cache_file_path() -> PathBuf {
        home_dir()
            .join(".local")
            .join("state")
            .join("cs-rs")
            .join("session-cache-v1.json")
    }

    fn load_cache() -> SessionCache {
        let path = Self::cache_file_path();
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(_) => {
                return SessionCache {
                    version: 1,
                    ..SessionCache::default()
                };
            }
        };

        match serde_json::from_str::<SessionCache>(&raw) {
            Ok(mut cache) => {
                if cache.version != 1 {
                    cache = SessionCache::default();
                    cache.version = 1;
                }
                cache
            }
            Err(_) => SessionCache {
                version: 1,
                ..SessionCache::default()
            },
        }
    }

    fn save_cache_if_dirty(&mut self) {
        if !self.cache_dirty {
            return;
        }
        self.save_cache();
        self.cache_dirty = false;
    }

    fn save_cache(&self) {
        let cache_path = Self::cache_file_path();
        let Some(parent) = cache_path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(raw) = serde_json::to_string_pretty(&self.cache) else {
            return;
        };
        let tmp_path = parent.join("session-cache-v1.json.tmp");
        if fs::write(&tmp_path, raw).is_err() {
            return;
        }
        let _ = fs::rename(tmp_path, cache_path);
    }

    fn metadata_modified_ms(metadata: &fs::Metadata) -> Option<i64> {
        metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
    }

    fn search_text_signature(file_path: Option<&str>) -> (u64, i64) {
        let Some(path) = file_path else {
            return (0, 0);
        };
        let Ok(metadata) = fs::metadata(path) else {
            return (0, 0);
        };
        let file_size = metadata.len();
        let file_modified_ms = Self::metadata_modified_ms(&metadata).unwrap_or(0);
        (file_size, file_modified_ms)
    }

    fn parse_history_lines_into(
        &self,
        source: SessionSource,
        history_path: &Path,
        start_offset: u64,
        seen: &mut HashMap<String, SessionInfo>,
    ) -> u64 {
        let file = match File::open(history_path) {
            Ok(file) => file,
            Err(_) => return 0,
        };
        let mut reader = BufReader::new(file);
        if start_offset > 0 && reader.seek(SeekFrom::Start(start_offset)).is_err() {
            return 0;
        }

        let mut parsed_lines = 0u64;
        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            parsed_lines += 1;

            let entry: HistoryEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let session_id = match entry.session_id.or(entry.session_id_legacy) {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };

            let display = if !entry.display.is_empty() {
                entry.display
            } else {
                entry.text
            };

            let project = entry.project;
            let timestamp = normalize_timestamp(entry.timestamp.or(entry.ts));

            let key = source.internal_key(&session_id);
            match seen.get_mut(&key) {
                Some(existing) => {
                    if timestamp > existing.timestamp {
                        existing.timestamp = timestamp;
                        existing.display = display.clone();
                        existing.project = project.clone();
                    } else {
                        if existing.display.is_empty() && !display.is_empty() {
                            existing.display = display.clone();
                        }
                        if existing.project.is_empty() && !project.is_empty() {
                            existing.project = project.clone();
                        }
                    }
                }
                None => {
                    seen.insert(
                        source.internal_key(&session_id),
                        SessionInfo {
                            source,
                            session_id,
                            display,
                            project,
                            timestamp,
                            model: String::new(),
                            reasoning_effort: String::new(),
                            file_path: None,
                        },
                    );
                }
            }
        }
        parsed_lines
    }

    fn load_sessions_for_source(&mut self, source: SessionSource) -> HashMap<String, SessionInfo> {
        let mut seen: HashMap<String, SessionInfo> = HashMap::new();
        let history_path = source.history_file();
        let cache_key = source.cache_key().to_string();
        if !history_path.exists() {
            if let Some(cached) = self.cache.histories.get(&cache_key) {
                for session in &cached.sessions {
                    seen.insert(source.internal_key(&session.session_id), session.clone());
                }
            }
            return seen;
        }

        let metadata = match fs::metadata(&history_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                if let Some(cached) = self.cache.histories.get(&cache_key) {
                    for session in &cached.sessions {
                        seen.insert(source.internal_key(&session.session_id), session.clone());
                    }
                }
                return seen;
            }
        };
        let file_size = metadata.len();
        let file_modified_ms = Self::metadata_modified_ms(&metadata).unwrap_or(0);
        let cached = self.cache.histories.get(&cache_key).cloned();

        if let Some(cached) = cached {
            if cached.file_size == file_size && cached.file_modified_ms == file_modified_ms {
                let looks_consistent =
                    cached.file_size == 0 || cached.line_count >= cached.sessions.len() as u64;
                if looks_consistent {
                    for session in cached.sessions {
                        seen.insert(source.internal_key(&session.session_id), session);
                    }
                    return seen;
                }
            }

            if file_size > cached.file_size && file_modified_ms >= cached.file_modified_ms {
                for session in cached.sessions {
                    seen.insert(source.internal_key(&session.session_id), session);
                }
                let appended = self.parse_history_lines_into(
                    source,
                    &history_path,
                    cached.file_size,
                    &mut seen,
                );
                self.cache.histories.insert(
                    cache_key,
                    CachedHistory {
                        file_size,
                        file_modified_ms,
                        line_count: cached.line_count.saturating_add(appended),
                        sessions: seen.values().cloned().collect(),
                    },
                );
                self.cache_dirty = true;
                return seen;
            }
        }

        let line_count = self.parse_history_lines_into(source, &history_path, 0, &mut seen);
        self.cache.histories.insert(
            cache_key,
            CachedHistory {
                file_size,
                file_modified_ms,
                line_count,
                sessions: seen.values().cloned().collect(),
            },
        );
        self.cache_dirty = true;
        seen
    }

    fn is_resumable_session(session: &SessionInfo) -> bool {
        if session.project.trim().is_empty() {
            return false;
        }
        session
            .file_path
            .as_deref()
            .map(Path::new)
            .map(Path::is_file)
            .unwrap_or(false)
    }

    fn codex_session_file_changed(&self, session_id: &str, path: &Path) -> bool {
        let Some(cached) = self.cache.codex_sessions.get(session_id) else {
            return true;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        let file_size = metadata.len();
        let file_modified_ms = Self::metadata_modified_ms(&metadata).unwrap_or(0);
        file_size != cached.file_size || file_modified_ms != cached.file_modified_ms
    }

    fn clear_codex_cache_path(&mut self, session_id: &str) {
        if let Some(entry) = self.cache.codex_sessions.get_mut(session_id) {
            if !entry.file_path.is_empty() {
                entry.file_path.clear();
                self.cache_dirty = true;
            }
        }
    }

    fn most_recent_model_for_source(
        &self,
        source: SessionSource,
        exclude_session_id: &str,
    ) -> Option<String> {
        self.sessions
            .values()
            .filter(|session| {
                session.source == source
                    && session.session_id != exclude_session_id
                    && !session.model.trim().is_empty()
            })
            .max_by_key(|session| session.timestamp)
            .map(|session| session.model.clone())
    }

    fn update_history_cache_session(&mut self, session: &SessionInfo) {
        let Some(history) = self.cache.histories.get_mut(session.source.cache_key()) else {
            return;
        };
        let Some(cached) = history
            .sessions
            .iter_mut()
            .find(|cached| cached.session_id == session.session_id)
        else {
            return;
        };

        if cached.display != session.display
            || cached.project != session.project
            || cached.timestamp != session.timestamp
            || cached.model != session.model
            || cached.reasoning_effort != session.reasoning_effort
            || cached.file_path != session.file_path
        {
            *cached = session.clone();
            self.cache_dirty = true;
        }
    }

    fn apply_cached_codex_metadata(&self, session: &mut SessionInfo) {
        let Some(cached) = self.cache.codex_sessions.get(&session.session_id) else {
            return;
        };

        if !cached.file_path.is_empty() {
            session.file_path = Some(cached.file_path.clone());
        }
        if session.project.is_empty() {
            if let Some(cwd) = cached.cwd.as_deref() {
                if !cwd.is_empty() {
                    session.project = cwd.to_string();
                }
            }
        }
        if session.timestamp == 0 {
            session.timestamp = cached.timestamp_ms.unwrap_or(0);
        }
        if session.model.is_empty() {
            if let Some(model) = cached.model.as_deref() {
                session.model = model.to_string();
            }
        }
        if session.reasoning_effort.is_empty() {
            if let Some(reasoning_effort) = cached.reasoning_effort.as_deref() {
                session.reasoning_effort = reasoning_effort.to_string();
            }
        }
    }

    fn update_codex_cache(
        &mut self,
        session_id: &str,
        path: &Path,
        info: Option<&CodexSessionFileInfo>,
    ) {
        let metadata = fs::metadata(path).ok();
        let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let file_modified_ms = metadata
            .as_ref()
            .and_then(Self::metadata_modified_ms)
            .unwrap_or(0);

        let mut entry = self
            .cache
            .codex_sessions
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        entry.file_path = path.to_string_lossy().to_string();
        entry.file_size = file_size;
        entry.file_modified_ms = file_modified_ms;
        if let Some(info) = info {
            if let Some(cwd) = info.cwd.as_ref() {
                if !cwd.is_empty() {
                    entry.cwd = Some(cwd.clone());
                }
            }
            if info.timestamp_ms.is_some() {
                entry.timestamp_ms = info.timestamp_ms;
            }
            if let Some(model) = info.model.as_ref() {
                entry.model = Some(model.clone());
            }
            if let Some(reasoning_effort) = info.reasoning_effort.as_ref() {
                entry.reasoning_effort = Some(reasoning_effort.clone());
            }
        }
        self.cache
            .codex_sessions
            .insert(session_id.to_string(), entry);
        self.cache_dirty = true;
    }

    fn load(&mut self) {
        if self.loaded {
            return;
        }

        let mut seen: HashMap<String, SessionInfo> = HashMap::new();
        let mut recent_codex_file_index: Option<HashMap<String, PathBuf>> = None;
        for source in SessionSource::all() {
            for (key, session) in self.load_sessions_for_source(*source) {
                seen.insert(key, session);
            }
        }

        for session in seen.values_mut() {
            match session.source {
                SessionSource::Claudecode => {
                    if !session.project.is_empty() {
                        let candidate = session
                            .source
                            .projects_dir()
                            .join(Self::encode_path(&session.project))
                            .join(format!("{}.jsonl", session.session_id));
                        if candidate.exists() {
                            session.file_path = Some(candidate.to_string_lossy().to_string());
                        }
                    }
                }
                SessionSource::Codex => {
                    self.apply_cached_codex_metadata(session);
                    if let Some(path) = session.file_path.as_deref() {
                        if !Path::new(path).is_file() {
                            session.file_path = None;
                            self.clear_codex_cache_path(&session.session_id);
                        }
                    }

                    let needs_fast_enrich = session.file_path.is_none()
                        || session.project.is_empty()
                        || session.timestamp == 0;
                    if needs_fast_enrich {
                        let index = recent_codex_file_index
                            .get_or_insert_with(|| Self::build_recent_codex_file_index(21));
                        if let Some(path) = index.get(&session.session_id) {
                            if session.file_path.is_none() {
                                session.file_path = Some(path.to_string_lossy().to_string());
                            }

                            let needs_meta = session.project.is_empty() || session.timestamp == 0;
                            if needs_meta {
                                if let Some(info) = self
                                    .codex_file_info_from_session_file(path, &session.session_id)
                                {
                                    if session.project.is_empty() {
                                        if let Some(cwd) = info.cwd.as_deref() {
                                            if !cwd.is_empty() {
                                                session.project = cwd.to_string();
                                            }
                                        }
                                    }
                                    if session.timestamp == 0 {
                                        session.timestamp = info.timestamp_ms.unwrap_or(0);
                                    }
                                    if session.model.is_empty() {
                                        if let Some(model) = info.model.as_deref() {
                                            session.model = model.to_string();
                                        }
                                    }
                                    if session.reasoning_effort.is_empty() {
                                        if let Some(reasoning_effort) =
                                            info.reasoning_effort.as_deref()
                                        {
                                            session.reasoning_effort = reasoning_effort.to_string();
                                        }
                                    }
                                    self.update_codex_cache(&session.session_id, path, Some(&info));
                                } else {
                                    self.update_codex_cache(&session.session_id, path, None);
                                }
                            } else {
                                self.update_codex_cache(&session.session_id, path, None);
                            }
                        }
                    }
                }
            }

            if session.display.is_empty() {
                session.display = session.project.clone();
            }
        }

        seen.retain(|_, session| {
            !(session.display.is_empty() && session.timestamp == 0)
                && Self::is_resumable_session(session)
        });
        self.sessions = seen;
        self.loaded = true;
        self.save_cache_if_dirty();
    }

    fn enrich_session_for_access(&mut self, source: SessionSource, session_id: &str) {
        self.load();
        let key = source.internal_key(session_id);
        let Some(mut session) = self.sessions.get(&key).cloned() else {
            return;
        };
        let mut session_changed = false;

        if let Some(path) = session.file_path.as_deref() {
            if !Path::new(path).is_file() {
                session.file_path = None;
                session_changed = true;
                if source == SessionSource::Codex {
                    self.clear_codex_cache_path(session_id);
                }
            }
        }

        if session.file_path.is_none() {
            if let Some(path) = self.find_session_file(source, session_id, &session.project) {
                session.file_path = Some(path.to_string_lossy().to_string());
                session_changed = true;
                if source == SessionSource::Codex {
                    self.update_codex_cache(session_id, &path, None);
                }
            }
        }

        if source == SessionSource::Codex {
            if let Some(path) = session.file_path.as_deref().map(Path::new) {
                let file_changed = self.codex_session_file_changed(session_id, path);
                let needs_meta = file_changed
                    || session.project.is_empty()
                    || session.timestamp == 0
                    || session.model.is_empty()
                    || session.reasoning_effort.is_empty();
                if needs_meta {
                    if let Some(info) = self.codex_file_info_from_session_file(path, session_id) {
                        if session.project.is_empty() {
                            if let Some(cwd) = info.cwd.as_deref() {
                                if !cwd.is_empty() {
                                    session.project = cwd.to_string();
                                    session_changed = true;
                                }
                            }
                        }
                        if session.timestamp == 0 {
                            session.timestamp = info.timestamp_ms.unwrap_or(0);
                            session_changed = true;
                        }
                        if let Some(model) = info.model.as_deref() {
                            if (file_changed || session.model.is_empty()) && session.model != model
                            {
                                session.model = model.to_string();
                                session_changed = true;
                            }
                        }
                        if let Some(reasoning_effort) = info.reasoning_effort.as_deref() {
                            if (file_changed || session.reasoning_effort.is_empty())
                                && session.reasoning_effort != reasoning_effort
                            {
                                session.reasoning_effort = reasoning_effort.to_string();
                                session_changed = true;
                            }
                        }
                        self.update_codex_cache(session_id, path, Some(&info));
                    }
                }
            }
        }

        if source == SessionSource::Claudecode {
            if let Some(path) = session.file_path.as_deref().map(Path::new) {
                if let Some(model) = Self::claudecode_model_from_session_file(path) {
                    if session.model != model {
                        session.model = model;
                        session_changed = true;
                    }
                }
            }
        }

        if session.display.is_empty() {
            session.display = session.project.clone();
            session_changed = true;
        }

        if session_changed {
            self.update_history_cache_session(&session);
            self.sessions.insert(key, session);
        }
    }

    fn all(&mut self) -> Vec<SessionInfo> {
        self.load();
        let mut out: Vec<_> = self.sessions.values().cloned().collect();
        out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        out
    }

    fn get_exact(&mut self, source: SessionSource, session_id: &str) -> Option<SessionInfo> {
        self.load();
        self.enrich_session_for_access(source, session_id);
        self.save_cache_if_dirty();
        let mut session = self
            .sessions
            .get(&source.internal_key(session_id))
            .cloned()?;
        if session.model.trim().is_empty() {
            if let Some(model) = self.most_recent_model_for_source(source, &session.session_id) {
                session.model = model;
            }
        }
        Some(session)
    }

    fn get(&mut self, session_id: &str) -> Option<SessionInfo> {
        self.load();
        let mut exact_matches = Vec::new();
        let mut matches = Vec::new();
        for session in self.sessions.values() {
            if session.session_id == session_id {
                exact_matches.push(session.clone());
            } else if session.session_id.starts_with(session_id) {
                matches.push(session.clone());
            }
        }

        if exact_matches.len() == 1 {
            let session = exact_matches.into_iter().next()?;
            return self.get_exact(session.source, &session.session_id);
        }
        if !exact_matches.is_empty() {
            return None;
        }

        if matches.len() == 1 {
            let session = matches.into_iter().next()?;
            return self.get_exact(session.source, &session.session_id);
        }
        None
    }

    fn find_session_file(
        &self,
        source: SessionSource,
        session_id: &str,
        project: &str,
    ) -> Option<PathBuf> {
        if !project.is_empty() {
            let encoded = Self::encode_path(project);
            let candidate = source
                .projects_dir()
                .join(encoded)
                .join(format!("{session_id}.jsonl"));
            if candidate.exists() {
                return Some(candidate);
            }
        }

        if source == SessionSource::Codex {
            if let Some(found) =
                Self::find_file_by_session_id(&source.sessions_dir(), session_id, 4)
            {
                return Some(found);
            }

            if let Some(found) =
                Self::find_file_by_session_id(&source.archived_sessions_dir(), session_id, 4)
            {
                return Some(found);
            }
        }

        let projects_dir = source.projects_dir();
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

    fn find_file_by_session_id(
        dir: &Path,
        session_id: &str,
        depth_remaining: usize,
    ) -> Option<PathBuf> {
        if depth_remaining == 0 || !dir.exists() {
            return None;
        }

        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.filter_map(Result::ok) {
            let p = entry.path();
            if p.is_file() {
                let is_jsonl = p.extension().and_then(|ext| ext.to_str()) == Some("jsonl");
                if !is_jsonl {
                    continue;
                }

                let name = p.file_name().and_then(|name| name.to_str()).unwrap_or("");
                if name.contains(session_id) {
                    return Some(p);
                }
                continue;
            }

            if p.is_dir() {
                if let Some(found) =
                    Self::find_file_by_session_id(&p, session_id, depth_remaining - 1)
                {
                    return Some(found);
                }
            }
        }

        None
    }

    fn sorted_child_dirs_desc(dir: &Path) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return dirs;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        dirs.sort_by(|a, b| {
            let a_name = a.file_name().and_then(|v| v.to_str()).unwrap_or("");
            let b_name = b.file_name().and_then(|v| v.to_str()).unwrap_or("");
            b_name.cmp(a_name)
        });
        dirs
    }

    fn looks_like_session_id(value: &str) -> bool {
        if value.len() != 36 {
            return false;
        }
        for (idx, ch) in value.chars().enumerate() {
            let is_hyphen = matches!(idx, 8 | 13 | 18 | 23);
            if is_hyphen {
                if ch != '-' {
                    return false;
                }
                continue;
            }
            if !ch.is_ascii_hexdigit() {
                return false;
            }
        }
        true
    }

    fn session_id_from_file_name(path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".jsonl")?;
        if stem.len() < 36 {
            return None;
        }
        let candidate = &stem[stem.len() - 36..];
        if Self::looks_like_session_id(candidate) {
            Some(candidate.to_string())
        } else {
            None
        }
    }

    fn recent_codex_day_dirs(limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let root = SessionSource::Codex.sessions_dir();
        for year in Self::sorted_child_dirs_desc(&root) {
            for month in Self::sorted_child_dirs_desc(&year) {
                for day in Self::sorted_child_dirs_desc(&month) {
                    out.push(day);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    fn build_recent_codex_file_index(day_dir_limit: usize) -> HashMap<String, PathBuf> {
        let mut index = HashMap::new();

        for day_dir in Self::recent_codex_day_dirs(day_dir_limit) {
            let Ok(entries) = fs::read_dir(day_dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Some(session_id) = Self::session_id_from_file_name(&path) {
                    index.entry(session_id).or_insert(path);
                }
            }
        }

        let archived = SessionSource::Codex.archived_sessions_dir();
        let Ok(entries) = fs::read_dir(&archived) else {
            return index;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() {
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Some(session_id) = Self::session_id_from_file_name(&path) {
                    index.entry(session_id).or_insert(path);
                }
            } else if path.is_dir() {
                let Ok(inner) = fs::read_dir(path) else {
                    continue;
                };
                for nested in inner.filter_map(Result::ok) {
                    let nested_path = nested.path();
                    if !nested_path.is_file() {
                        continue;
                    }
                    if nested_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(session_id) = Self::session_id_from_file_name(&nested_path) {
                        index.entry(session_id).or_insert(nested_path);
                    }
                }
            }
        }

        index
    }

    fn codex_file_info_from_session_file(
        &self,
        path: &Path,
        expected_session_id: &str,
    ) -> Option<CodexSessionFileInfo> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let mut out = CodexSessionFileInfo::default();
        let mut saw_session_meta = false;
        let mut current_session_matches = true;

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let entry_type = value.get("type").and_then(Value::as_str);
            if entry_type == Some("session_meta") {
                saw_session_meta = true;
                let payload = match value.get("payload") {
                    Some(v) => v,
                    None => continue,
                };

                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                current_session_matches = id.is_empty() || id == expected_session_id;
                if !id.is_empty() && id != expected_session_id {
                    continue;
                }

                let parsed: CodexSessionMeta = serde_json::from_value(payload.clone()).ok()?;
                if !parsed.id.is_empty() && !id.is_empty() && parsed.id != id {
                    continue;
                }

                let cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !cwd.is_empty() {
                    out.cwd = Some(cwd);
                }

                let ts = payload
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(codex_iso_to_ms)
                    .or_else(|| codex_iso_to_ms(&parsed.timestamp));
                if let Some(ts) = ts {
                    out.timestamp_ms = Some(ts);
                }
                continue;
            }

            let line_session_id = codex_entry_session_id(&value);
            if let Some(found_session_id) = line_session_id {
                if found_session_id != expected_session_id {
                    continue;
                }
            } else if saw_session_meta && !current_session_matches {
                continue;
            }

            if entry_type == Some("turn_context") {
                let payload = match value.get("payload") {
                    Some(v) => v,
                    None => continue,
                };
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    if let Some(model) = codex_model_candidate(model) {
                        out.model = Some(model);
                    }
                }
                if let Some(reasoning_effort) = payload
                    .get("effort")
                    .and_then(Value::as_str)
                    .and_then(codex_effort_candidate)
                {
                    out.reasoning_effort = Some(reasoning_effort);
                }
                if out.reasoning_effort.is_none() {
                    if let Some(reasoning_effort) = payload
                        .get("collaboration_mode")
                        .and_then(|v| v.get("settings"))
                        .and_then(|v| v.get("reasoning_effort"))
                        .and_then(Value::as_str)
                        .and_then(codex_effort_candidate)
                    {
                        out.reasoning_effort = Some(reasoning_effort);
                    }
                }
                continue;
            }

            if entry_type == Some("response_item") {
                let payload = match value.get("payload") {
                    Some(v) => v,
                    None => continue,
                };
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    if let Some(model) = codex_model_candidate(model) {
                        out.model = Some(model);
                    }
                }
            }
        }

        if out.cwd.is_none()
            && out.timestamp_ms.is_none()
            && out.model.is_none()
            && out.reasoning_effort.is_none()
        {
            None
        } else {
            Some(out)
        }
    }

    fn claudecode_model_from_session_file(path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let mut latest_model = None;

        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let candidate = value
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .and_then(codex_model_candidate);
            if let Some(model) = candidate {
                latest_model = Some(model);
            }
        }

        latest_model
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

            let msg = match session.source {
                SessionSource::Claudecode => match serde_json::from_str::<RawMessage>(line) {
                    Ok(raw) => Some(Message::from(raw)),
                    Err(_) => None,
                },
                SessionSource::Codex => parse_codex_message(line),
            };
            let msg = match msg {
                Some(msg) => msg,
                None => continue,
            };
            if skip_internal && INTERNAL_TYPES.contains(&msg.msg_type.as_str()) {
                continue;
            }
            out.push(msg);
        }
        out
    }

    fn session_contains_full_text(&mut self, session: &SessionInfo, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }

        let key = session.source.internal_key(&session.session_id);
        let (file_size, file_modified_ms) =
            Self::search_text_signature(session.file_path.as_deref());

        let cache_miss_or_stale = match self.search_text_cache.get(&key) {
            Some(cached) => {
                cached.file_size != file_size || cached.file_modified_ms != file_modified_ms
            }
            None => true,
        };

        if cache_miss_or_stale {
            let text = self
                .read_messages(session, true)
                .into_iter()
                .map(|msg| msg.text())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();

            self.search_text_cache.insert(
                key.clone(),
                SearchTextCacheEntry {
                    file_size,
                    file_modified_ms,
                    text,
                },
            );
        }

        self.search_text_cache
            .get(&key)
            .map(|cached| cached.text.contains(query))
            .unwrap_or(false)
    }

    fn search(
        &mut self,
        query: &str,
        project: Option<&str>,
        max_results: usize,
    ) -> Result<Vec<(SessionInfo, Message, String)>> {
        self.load();

        let pattern =
            Regex::new(&format!("(?i){query}")).map_err(|err| anyhow!("invalid regex: {err}"))?;

        let mut results: Vec<(SessionInfo, Message, String)> = Vec::new();
        for session in self.all() {
            self.enrich_session_for_access(session.source, &session.session_id);
            let session = self
                .sessions
                .get(&session.source.internal_key(&session.session_id))
                .cloned()
                .unwrap_or(session);

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
                        self.save_cache_if_dirty();
                        return Ok(results);
                    }
                    break;
                }
            }
        }

        self.save_cache_if_dirty();
        Ok(results)
    }

    fn build_stats_report(&mut self) -> StatsReport {
        self.load();

        // Stats are the one place we can pay a little extra cost to enrich missing
        // model metadata without impacting startup/list latency.
        let session_keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in session_keys {
            let Some(session) = self.sessions.get(&key).cloned() else {
                continue;
            };
            if session.source != SessionSource::Claudecode || !session.model.trim().is_empty() {
                continue;
            }
            let Some(path) = session.file_path.as_deref().map(Path::new) else {
                continue;
            };
            if let Some(model) = Self::claudecode_model_from_session_file(path) {
                let mut updated_session = None;
                if let Some(target) = self.sessions.get_mut(&key) {
                    if target.model != model {
                        target.model = model;
                        updated_session = Some(target.clone());
                    }
                }
                if let Some(updated) = updated_session {
                    self.update_history_cache_session(&updated);
                }
            }
        }

        let total_sessions = self.sessions.len() as u64;
        let total_history_entries = SessionSource::all()
            .iter()
            .map(|source| {
                self.cache
                    .histories
                    .get(source.cache_key())
                    .map(|h| h.line_count)
                    .unwrap_or(0)
            })
            .sum::<u64>();

        let last_computed_date = Local::now().format("%Y-%m-%d").to_string();

        let mut sources = Vec::new();
        for source in SessionSource::all() {
            let mut sessions = 0u64;
            let mut first_session_ts: Option<i64> = None;
            let mut model_counts: HashMap<String, u64> = HashMap::new();
            let mut daily_sessions: BTreeMap<String, u64> = BTreeMap::new();

            for session in self.sessions.values().filter(|s| s.source == *source) {
                sessions += 1;
                if session.timestamp > 0 {
                    first_session_ts = Some(
                        first_session_ts
                            .map(|existing| existing.min(session.timestamp))
                            .unwrap_or(session.timestamp),
                    );
                    if let Some(ts) = Local.timestamp_millis_opt(session.timestamp).single() {
                        let day = ts.format("%Y-%m-%d").to_string();
                        *daily_sessions.entry(day).or_insert(0) += 1;
                    }
                }
                if !session.model.trim().is_empty() {
                    *model_counts.entry(session.model.clone()).or_insert(0) += 1;
                }
            }

            let history_entries = self
                .cache
                .histories
                .get(source.cache_key())
                .map(|h| h.line_count)
                .unwrap_or(0);

            let mut top_models: Vec<(String, u64)> = model_counts.into_iter().collect();
            top_models.sort_by(|a, b| b.1.cmp(&a.1));
            top_models.truncate(8);

            let mut daily_sessions: Vec<(String, u64)> = daily_sessions.into_iter().collect();
            if daily_sessions.len() > 14 {
                let keep_from = daily_sessions.len() - 14;
                daily_sessions = daily_sessions.into_iter().skip(keep_from).collect();
            }

            let first_session_date = first_session_ts
                .and_then(|ts| Local.timestamp_millis_opt(ts).single())
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "—".to_string());

            sources.push(StatsSourceRow {
                source: *source,
                sessions,
                history_entries,
                first_session_date,
                top_models,
                daily_sessions,
            });
        }

        let report = StatsReport {
            total_sessions,
            total_history_entries,
            last_computed_date,
            sources,
        };
        self.save_cache_if_dirty();
        report
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

    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if hrs < 24 {
        format!("{hrs}h ago")
    } else {
        when.format("%Y-%m-%d").to_string()
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

fn session_id_hex_tail(session_id: &str, count: usize) -> String {
    let hex_chars: Vec<char> = session_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex_chars.len() >= count {
        return hex_chars[hex_chars.len() - count..].iter().collect();
    }

    session_id
        .chars()
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn render_conversation(
    store: &SessionStore,
    session: &SessionInfo,
    thinking: bool,
    tail: Option<usize>,
) -> Vec<String> {
    let assistant_label = if session.source == SessionSource::Codex {
        "Codex"
    } else {
        "Claude"
    };
    let mut lines = Vec::new();
    lines.push(format!("Session: {}", truncate(&session.display, 120)));
    lines.push(format!("Source: {}", session.source.list_label()));
    lines.push(format!("Session ID (full): {}", session.session_id));
    lines.push(format!(
        "{}  ·  {}",
        short_project(&session.project),
        relative_time(session.timestamp),
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
                if matches!(btype, "text" | "input_text" | "output_text") {
                    let text = block_text(&block).unwrap_or_default();
                    if !text.trim().is_empty() {
                        parts.push(text);
                    }
                } else if btype == "tool_use" {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let input = block.get("input").unwrap_or(&Value::Null);
                    let summary = match name {
                        "Bash" => {
                            let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
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
                            let desc = input
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            format!("Task {desc}")
                        }
                        "WebSearch" => {
                            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
                            format!("Search: {query}")
                        }
                        _ => format!("{name}(...)"),
                    };
                    parts.push(format!("[tool] {summary}"));
                } else if btype == "thinking" && thinking {
                    let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                    if !thinking.trim().is_empty() {
                        parts.push(format!("[thinking] {}", truncate(thinking, 250)));
                    }
                }
            }

            if !parts.is_empty() {
                let model = msg.model();
                if model.is_empty() {
                    lines.push(format!("{assistant_label}: {}", parts.join("\n")));
                } else if model != "<synthetic>" {
                    lines.push(format!("{assistant_label} ({model}): {}", parts.join("\n")));
                } else {
                    lines.push(format!("{assistant_label}: {}", parts.join("\n")));
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
        let assistant_label = if session.source == SessionSource::Codex {
            "Codex"
        } else {
            "Claude"
        };
        out.push_str(&format!(
            "{}  {}  {}\n",
            session.short_id(),
            relative_time(session.timestamp),
            short_project(&session.project)
        ));
        out.push_str(&format!("  {}\n", truncate(&session.display, 80)));
        let role = msg.role();
        let role_label = if role == "user" {
            "You"
        } else {
            assistant_label
        };
        out.push_str(&format!("  {role_label}: {}\n\n", truncate(&line, 100)));
    }
    out
}

fn render_stats(stats: &StatsReport) -> String {
    fn render_bar(count: u64, max_count: u64, width: usize) -> String {
        if max_count == 0 || width == 0 {
            return String::new();
        }
        let n = ((count.saturating_mul(width as u64)) / max_count) as usize;
        "█".repeat(n.min(width))
    }

    let mut out = String::new();
    const FRAME_W: usize = 82;
    let title = "Session Usage Stats (Claude Code + Codex)";
    out.push_str(&format!("╭{}╮\n", "─".repeat(FRAME_W - 2)));
    out.push_str(&format!("│{:^width$}│\n", title, width = FRAME_W - 2));
    out.push_str(&format!("╰{}╯\n\n", "─".repeat(FRAME_W - 2)));

    out.push_str(&format!(
        "Total sessions: {}\n",
        format_with_commas(stats.total_sessions)
    ));
    out.push_str(&format!(
        "Total history entries: {}\n",
        format_with_commas(stats.total_history_entries)
    ));
    out.push_str(&format!("Last computed: {}\n", stats.last_computed_date));
    out.push('\n');

    for row in &stats.sources {
        out.push_str(&format!("{}:\n", row.source.label().to_uppercase()));
        out.push_str(&format!(
            "  Sessions: {}\n",
            format_with_commas(row.sessions),
        ));
        out.push_str(&format!(
            "  History entries: {}\n",
            format_with_commas(row.history_entries),
        ));
        out.push_str(&format!("  First session: {}\n", row.first_session_date));
        out.push('\n');

        if row.top_models.is_empty() {
            out.push_str("  Top models (session-level): —\n");
        } else {
            out.push_str("  Top models (session-level):\n");
            for (model, count) in &row.top_models {
                out.push_str(&format!(
                    "    {:<34} {}\n",
                    truncate(model, 34),
                    format_with_commas(*count)
                ));
            }
        }
        out.push('\n');

        if !row.daily_sessions.is_empty() {
            let max_sessions = row
                .daily_sessions
                .iter()
                .map(|(_, count)| *count)
                .max()
                .unwrap_or(1);
            out.push_str("  Daily sessions (last 14 days):\n");
            for (date, count) in &row.daily_sessions {
                let bar = render_bar(*count, max_sessions, 24);
                out.push_str(&format!(
                    "    {} {:>6} {}\n",
                    date,
                    format_with_commas(*count),
                    bar
                ));
            }
            out.push('\n');
        } else {
            out.push_str("  Daily sessions (last 14 days): —\n\n");
        }
        out.push_str(&format!("{}\n\n", "-".repeat(FRAME_W)));
    }

    out
}

fn list_sessions(sessions: Vec<SessionInfo>, json_output: bool, max_count: usize) -> String {
    let mut out = String::new();
    let subset: Vec<_> = sessions.into_iter().take(max_count).collect();
    let rows: Vec<(SessionInfo, i64)> = subset
        .into_iter()
        .map(|s| {
            let ts_ms = list_time_ms_for_session(&s);
            (s, ts_ms)
        })
        .collect();

    if json_output {
        let data: Vec<_> = rows
            .into_iter()
            .map(|(s, _)| {
                serde_json::json!({
                    "source": s.source.label(),
                    "session_id": s.session_id,
                    "display": s.display,
                    "project": s.project,
                    "timestamp": s.timestamp,
                    "model": s.model,
                    "reasoning_effort": s.reasoning_effort,
                    "file_path": s.file_path,
                })
            })
            .collect();
        let value = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "[]".to_string());
        return value;
    }

    let source_width = rows
        .iter()
        .map(|(s, _)| s.source.list_label().len())
        .max()
        .unwrap_or(6)
        .max("source".len());
    let time_width = rows
        .iter()
        .map(|(_, ts_ms)| list_time(*ts_ms).len())
        .max()
        .unwrap_or(4)
        .max("time".len());
    let project_width = rows
        .iter()
        .map(|(s, _)| short_project(&s.project).chars().count().min(32))
        .max()
        .unwrap_or(7)
        .max("project".len());
    let title_width = rows
        .iter()
        .map(|(s, _)| s.display.chars().count().min(48))
        .max()
        .unwrap_or(5)
        .max("title".len());

    out.push_str(&format!(
        "{: <source_width$}  {: <5}  {: <time_width$}  {: <project_width$}  {}\n",
        "source", "id5", "time", "project", "title"
    ));
    let line_width = source_width + 2 + 5 + 2 + time_width + 2 + project_width + 2 + title_width;
    out.push_str(&"-".repeat(line_width));
    out.push('\n');
    for (s, ts_ms) in rows {
        let short_id = s.list_id_tail();
        let time = list_time(ts_ms);
        let proj = truncate(&short_project(&s.project), project_width)
            .chars()
            .take(project_width)
            .collect::<String>();
        let title = truncate(&s.display, title_width);
        out.push_str(&format!(
            "{: <source_width$}  {short_id:5}  {time:<time_width$}  {proj:<project_width$}  {title}\n",
            s.source.list_label()
        ));
    }
    out
}

fn is_view_shortcut(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
}

fn open_selected_detail(
    store: &mut SessionStore,
    filtered: &[SessionInfo],
    list_state: &ListState,
    detail_lines: &mut Vec<String>,
    in_detail: &mut bool,
    detail_scroll: &mut usize,
) {
    let idx = list_state.selected().unwrap_or_default();
    if idx >= filtered.len() {
        return;
    }

    let selected = &filtered[idx];
    let session = store
        .get_exact(selected.source, &selected.session_id)
        .unwrap_or_else(|| selected.clone());
    *detail_lines = render_conversation(store, &session, false, None);
    *in_detail = true;
    *detail_scroll = 0;
}

fn refresh_filter_results(
    store: &mut SessionStore,
    filtered: &mut Vec<SessionInfo>,
    sessions: &[SessionInfo],
    previous_filter: &mut String,
    filter: &str,
) {
    let candidate_pool =
        if !previous_filter.is_empty() && filter.starts_with(previous_filter.as_str()) {
            filtered.clone()
        } else {
            sessions.to_vec()
        };
    apply_filter(store, filtered, &candidate_pool, filter);
    previous_filter.clear();
    previous_filter.push_str(filter);
}

fn run_tui() -> Result<()> {
    let mut store = SessionStore::new();
    let mut sessions = store.all();
    let list_time_by_session = build_list_time_map(&sessions);
    sessions.sort_by_cached_key(|s| {
        Reverse(
            *list_time_by_session
                .get(&s.source.internal_key(&s.session_id))
                .unwrap_or(&s.timestamp),
        )
    });
    let mut filtered = sessions.clone();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let mut filter = String::new();
    let mut previous_filter = String::new();
    let mut filter_input = false;
    let mut in_detail = false;
    let mut detail_lines = Vec::<String>::new();
    let mut detail_scroll: usize = 0;

    let mut terminal = init_terminal()?;

    loop {
        terminal.draw(|f| {
            let size = f.size();
            let top_height = if in_detail {
                0u16
            } else {
                (filter_input as u16) + 1
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(top_height),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(size);

            let status = if in_detail {
                " [↑/↓] scroll  [Esc]/[b] back  [Ctrl-c]/[q] quit"
            } else {
                " [↑/↓ or Ctrl-u/Ctrl-d] navigate  [Enter] resume  [Option-v] view  [/] search  [Ctrl-c]/[q] quit"
            };

            if !in_detail {
                let filter_text = if filter_input {
                    format!("> {filter}")
                } else {
                    "Type to full-text search sessions...".to_string()
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
                let lines: Vec<Line> = slice.iter().map(|line| Line::from(line.clone())).collect();
                f.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title("Session")),
                    chunks[1],
                );
            } else {
                let items: Vec<ListItem> = filtered
                    .iter()
                    .map(|s| {
                        let time = list_time(
                            *list_time_by_session
                                .get(&s.source.internal_key(&s.session_id))
                                .unwrap_or(&s.timestamp),
                        );
                        let id_tail = s.list_id_tail();
                        let project = truncate(&short_project(&s.project), 24);
                        let (size, is_large_size) = file_size_for_session(&s.file_path);
                        let prompt_w = (chunks[1].width as usize)
                            .saturating_sub(16 + 3 + 5 + 3 + 5 + 3 + 24 + 3 + 8 + 3)
                            .max(20);
                        let prompt = truncate(&s.display, prompt_w);
                        let source = s.source.list_label();
                        let source_style = if s.source == SessionSource::Codex {
                            Style::default().fg(Color::Rgb(88, 166, 255))
                        } else {
                            // Anthropic-style orange for cc rows.
                            Style::default().fg(Color::Rgb(217, 119, 87))
                        };
                        let size_style = if is_large_size {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        let row = Line::from(vec![
                            Span::styled(
                                format!("{time:>16}"),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::from("   "),
                            Span::styled(format!("{source:5}"), source_style),
                            Span::from("   "),
                            Span::styled(format!("{id_tail:>5}"), Style::default().fg(Color::DarkGray)),
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
                Paragraph::new(status).style(Style::default().fg(Color::White)),
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

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
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

        if filter_input {
            if is_view_shortcut(&key) {
                open_selected_detail(
                    &mut store,
                    &filtered,
                    &list_state,
                    &mut detail_lines,
                    &mut in_detail,
                    &mut detail_scroll,
                );
                continue;
            }

            match key.code {
                KeyCode::Esc => {
                    filter_input = false;
                    filter.clear();
                    refresh_filter_results(
                        &mut store,
                        &mut filtered,
                        &sessions,
                        &mut previous_filter,
                        &filter,
                    );
                    list_state.select(Some(0));
                }
                KeyCode::Backspace => {
                    filter.pop();
                    refresh_filter_results(
                        &mut store,
                        &mut filtered,
                        &sessions,
                        &mut previous_filter,
                        &filter,
                    );
                    list_state.select(Some(0));
                }
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
                        None => 0,
                    };
                    list_state.select(Some(next));
                }
                KeyCode::Enter => {
                    let idx = list_state.selected().unwrap_or_default();
                    if idx < filtered.len() {
                        let selected = &filtered[idx];
                        let session = store
                            .get_exact(selected.source, &selected.session_id)
                            .unwrap_or_else(|| selected.clone());
                        cleanup_terminal(&mut terminal)?;
                        resume_session(&session)?;
                        return Ok(());
                    }
                }
                KeyCode::Char(c) => {
                    if !c.is_control() && key.modifiers.is_empty() {
                        filter.push(c);
                    }
                    refresh_filter_results(
                        &mut store,
                        &mut filtered,
                        &sessions,
                        &mut previous_filter,
                        &filter,
                    );
                    list_state.select(Some(0));
                }
                _ => {}
            }
            continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u') => {
                    let prev = match list_state.selected() {
                        Some(0) | None => 0,
                        Some(i) => i.saturating_sub(1),
                    };
                    list_state.select(Some(prev));
                    continue;
                }
                KeyCode::Char('d') => {
                    let len = filtered.len();
                    let next = match list_state.selected() {
                        Some(i) => {
                            if i + 1 < len {
                                i + 1
                            } else {
                                i
                            }
                        }
                        None => 0,
                    };
                    list_state.select(Some(next));
                    continue;
                }
                _ => {}
            }
        }

        if is_view_shortcut(&key) {
            open_selected_detail(
                &mut store,
                &filtered,
                &list_state,
                &mut detail_lines,
                &mut in_detail,
                &mut detail_scroll,
            );
            continue;
        }

        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('/') => {
                filter_input = true;
                filter.clear();
                refresh_filter_results(
                    &mut store,
                    &mut filtered,
                    &sessions,
                    &mut previous_filter,
                    &filter,
                );
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
                    None => 0,
                };
                list_state.select(Some(next));
            }
            KeyCode::Enter => {
                let idx = list_state.selected().unwrap_or_default();
                if idx < filtered.len() {
                    let selected = &filtered[idx];
                    let session = store
                        .get_exact(selected.source, &selected.session_id)
                        .unwrap_or_else(|| selected.clone());
                    cleanup_terminal(&mut terminal)?;
                    resume_session(&session)?;
                    return Ok(());
                }
            }
            _ => {}
        }
    }

    cleanup_terminal(&mut terminal)?;
    Ok(())
}

fn apply_filter(
    store: &mut SessionStore,
    filtered: &mut Vec<SessionInfo>,
    sessions: &[SessionInfo],
    filter: &str,
) {
    let q = filter.to_lowercase();
    if q.is_empty() {
        filtered.clear();
        filtered.extend_from_slice(sessions);
        return;
    }

    filtered.clear();
    for session in sessions {
        let source_label = session.source.label().to_lowercase();
        let source_list_label = session.source.list_label().to_lowercase();
        if session.display.to_lowercase().contains(&q)
            || session.project.to_lowercase().contains(&q)
            || session.session_id.to_lowercase().contains(&q)
            || source_label.contains(&q)
            || source_list_label.contains(&q)
            || store.session_contains_full_text(session, &q)
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

fn resolve_resume_cwd(session: &SessionInfo) -> Result<PathBuf> {
    let configured = Path::new(&session.project);
    if configured.as_os_str().is_empty() {
        return Err(anyhow!("Session project path is empty"));
    }

    if !configured.exists() {
        fs::create_dir_all(configured).with_context(|| {
            format!(
                "failed to create project directory {}",
                configured.display()
            )
        })?;
    }

    if configured.exists() {
        return Ok(configured.to_path_buf());
    }

    Err(anyhow!(
        "Failed to create or resolve session project directory: {}",
        configured.display()
    ))
}

fn resume_session(session: &SessionInfo) -> Result<()> {
    let session_id = shell_single_quote(&session.session_id);
    let resume_cmd = session.source.resume_command();
    let fallback = session.source.fallback_resume_command();
    let resume_invocation = session.source.resume_invocation();
    let model_arg = if let Some(model) = codex_model_candidate(&session.model) {
        format!(
            " {} {}",
            session.source.resume_model_flag(),
            shell_single_quote(&model)
        )
    } else {
        String::new()
    };
    let effort_arg = if session.source == SessionSource::Codex {
        codex_effort_candidate(&session.reasoning_effort)
            .map(|effort| {
                let config_pair = format!("model_reasoning_effort=\"{effort}\"");
                format!(" -c {}", shell_single_quote(&config_pair))
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let script = format!(
        "cs_session_id={session_id}; if whence -w {resume_cmd} >/dev/null 2>&1; then {resume_cmd} {invocation}{model_arg}{effort_arg}; elif whence -w {fallback} >/dev/null 2>&1; then {fallback} {invocation}{model_arg}{effort_arg}; fi",
        session_id = session_id,
        resume_cmd = resume_cmd,
        invocation = resume_invocation,
        fallback = fallback,
        model_arg = model_arg,
        effort_arg = effort_arg,
    );

    let mut cmd = Command::new("zsh");
    cmd.arg("-ic").arg(script);
    let project_path = resolve_resume_cwd(session)?;
    cmd.current_dir(project_path);
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

fn list_command(
    store: &mut SessionStore,
    project: Option<String>,
    since: Option<String>,
    limit: usize,
    json: bool,
) -> Result<String> {
    let mut sessions = store.all();

    if let Some(p) = project.as_deref() {
        let p = p.to_lowercase();
        sessions.retain(|s| s.project.to_lowercase().contains(&p));
    }

    if let Some(since_s) = since {
        let since_ms = chrono::NaiveDate::parse_from_str(&since_s, "%Y-%m-%d")
            .map(|date| {
                date.and_hms_opt(0, 0, 0)
                    .and_then(|naive| Local.from_local_datetime(&naive).single())
                    .map(|ts| ts.timestamp_millis())
            })
            .ok()
            .flatten()
            .with_context(|| format!("Invalid date format: {since_s} (use YYYY-MM-DD)"))?;

        sessions.retain(|s| s.timestamp >= since_ms);
    }

    sessions.sort_by_cached_key(|s| Reverse(list_time_ms_for_session(s)));
    Ok(list_sessions(sessions, json, limit))
}

#[derive(Parser)]
#[command(name = "cs-rs", about = "Session tools for Claude Code and Codex")]
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
        Some(Commands::Search {
            query,
            project,
            max,
        }) => {
            let results = store.search(&query, project.as_deref(), max)?;
            println!("{}", render_search_results(results));
        }
        Some(Commands::Stats) => {
            let stats = store.build_stats_report();
            println!("{}", render_stats(&stats));
        }
        Some(Commands::List {
            project,
            since,
            limit,
            json,
        }) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> SessionStore {
        SessionStore {
            sessions: HashMap::new(),
            loaded: true,
            cache: SessionCache {
                version: 1,
                ..SessionCache::default()
            },
            cache_dirty: false,
            search_text_cache: HashMap::new(),
        }
    }

    #[test]
    fn get_exact_falls_back_to_recent_source_model() {
        let source = SessionSource::Claudecode;
        let recent = SessionInfo {
            source,
            session_id: "recent-session".to_string(),
            display: "recent".to_string(),
            project: "/tmp/project".to_string(),
            timestamp: 2_000,
            model: "claude-opus-4-6".to_string(),
            reasoning_effort: String::new(),
            file_path: Some("/tmp/recent.jsonl".to_string()),
        };
        let target = SessionInfo {
            source,
            session_id: "target-session".to_string(),
            display: "target".to_string(),
            project: "/tmp/project".to_string(),
            timestamp: 1_000,
            model: String::new(),
            reasoning_effort: String::new(),
            file_path: Some("/tmp/target.jsonl".to_string()),
        };

        let mut store = test_store();
        store
            .sessions
            .insert(source.internal_key(&recent.session_id), recent);
        store
            .sessions
            .insert(source.internal_key(&target.session_id), target);

        let resolved = store
            .get_exact(source, "target-session")
            .expect("target session should resolve");
        assert_eq!(resolved.model, "claude-opus-4-6");
    }

    #[test]
    fn render_stats_outputs_separate_source_sections() {
        let report = StatsReport {
            total_sessions: 2,
            total_history_entries: 3,
            last_computed_date: "2026-02-13".to_string(),
            sources: vec![
                StatsSourceRow {
                    source: SessionSource::Claudecode,
                    sessions: 1,
                    history_entries: 2,
                    first_session_date: "2026-02-01".to_string(),
                    top_models: vec![("claude-opus-4-6".to_string(), 1)],
                    daily_sessions: vec![("2026-02-13".to_string(), 1)],
                },
                StatsSourceRow {
                    source: SessionSource::Codex,
                    sessions: 1,
                    history_entries: 1,
                    first_session_date: "2026-02-02".to_string(),
                    top_models: vec![("gpt-5.2-codex".to_string(), 1)],
                    daily_sessions: vec![("2026-02-13".to_string(), 1)],
                },
            ],
        };

        let rendered = render_stats(&report);
        assert!(rendered.contains("CLAUDE CODE:"));
        assert!(rendered.contains("CODEX:"));
        assert!(rendered.contains("claude-opus-4-6"));
        assert!(rendered.contains("gpt-5.2-codex"));
    }

    #[test]
    fn codex_model_candidate_normalizes_effort_suffix() {
        assert_eq!(
            codex_model_candidate("gpt-5.3-codex high").as_deref(),
            Some("gpt-5.3-codex")
        );
        assert_eq!(
            codex_model_candidate("  gpt-5.3-codex   medium ").as_deref(),
            Some("gpt-5.3-codex")
        );
    }

    #[test]
    fn codex_effort_candidate_normalizes_case() {
        assert_eq!(codex_effort_candidate("HIGH").as_deref(), Some("high"));
        assert_eq!(codex_effort_candidate("xhigh").as_deref(), Some("xhigh"));
        assert_eq!(codex_effort_candidate(""), None);
        assert_eq!(codex_effort_candidate("high effort"), None);
    }

    #[test]
    fn codex_file_info_extracts_model_and_effort() {
        let session_id = "019c24fb-6f78-7a20-99d0-88871c381f5d";
        let file_name = format!(
            "cs-rs-codex-info-test-{}-{}.jsonl",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = env::temp_dir().join(file_name);
        let fixture = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"timestamp\":\"2026-02-13T17:00:00.000Z\",\"cwd\":\"/tmp/demo\"}}}}\n\
{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.3-codex high\",\"effort\":\"HIGH\",\"collaboration_mode\":{{\"settings\":{{\"reasoning_effort\":\"medium\"}}}}}}}}\n"
        );
        fs::write(&path, fixture).expect("failed to write fixture file");

        let store = test_store();
        let info = store
            .codex_file_info_from_session_file(&path, session_id)
            .expect("expected codex file info");

        assert_eq!(info.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(info.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(info.cwd.as_deref(), Some("/tmp/demo"));
        assert_eq!(info.timestamp_ms, Some(1_771_002_000_000));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_id_hex_tail_uses_last_five_hex_chars() {
        let id = "019c24fb-6f78-7a20-99d0-88871c381f5d";
        assert_eq!(session_id_hex_tail(id, 5), "81f5d");
    }

    #[test]
    fn apply_filter_matches_cc_source_alias() {
        let mut store = test_store();
        let sessions = vec![
            SessionInfo {
                source: SessionSource::Claudecode,
                session_id: "a".to_string(),
                display: "one".to_string(),
                project: "/tmp/a".to_string(),
                timestamp: 1,
                model: String::new(),
                reasoning_effort: String::new(),
                file_path: None,
            },
            SessionInfo {
                source: SessionSource::Codex,
                session_id: "b".to_string(),
                display: "two".to_string(),
                project: "/tmp/b".to_string(),
                timestamp: 1,
                model: String::new(),
                reasoning_effort: String::new(),
                file_path: None,
            },
        ];
        let mut filtered = Vec::new();
        apply_filter(&mut store, &mut filtered, &sessions, "cc");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].source, SessionSource::Claudecode);
    }

    #[test]
    fn apply_filter_matches_full_text_in_session_messages() {
        let mut store = test_store();
        let session_id = "fulltext-session";
        let file_name = format!(
            "cs-rs-fulltext-filter-test-{}-{}.jsonl",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = env::temp_dir().join(file_name);
        let fixture = format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-02-13T17:00:00.000Z\",\"isApiErrorMessage\":false,\"sessionId\":\"{session_id}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"this includes flibbertigibbet\"}}]}}}}\n"
        );
        fs::write(&path, fixture).expect("failed to write fixture file");

        let sessions = vec![SessionInfo {
            source: SessionSource::Claudecode,
            session_id: session_id.to_string(),
            display: "session".to_string(),
            project: "/tmp/fulltext".to_string(),
            timestamp: 1,
            model: String::new(),
            reasoning_effort: String::new(),
            file_path: Some(path.to_string_lossy().to_string()),
        }];

        let mut filtered = Vec::new();
        apply_filter(&mut store, &mut filtered, &sessions, "flibbertigibbet");
        assert_eq!(filtered.len(), 1);
        assert_eq!(store.search_text_cache.len(), 1);

        apply_filter(&mut store, &mut filtered, &sessions, "flibber");
        assert_eq!(filtered.len(), 1);
        assert_eq!(store.search_text_cache.len(), 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn list_time_formats_older_values_with_date_and_clock_time() {
        let ts_ms = Local
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 0)
            .single()
            .expect("valid local datetime")
            .timestamp_millis();
        assert_eq!(list_time(ts_ms), "2026-01-02 03:04");
    }
}
