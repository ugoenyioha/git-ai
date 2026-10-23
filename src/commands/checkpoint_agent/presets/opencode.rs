use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct OpenCodePreset;

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    hook_event_name: String,
    session_id: String,
    cwd: String,
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
}

impl OpenCodePreset {
    pub(crate) fn extract_filepaths_from_tool_input(
        tool_input: Option<&serde_json::Value>,
        cwd: &str,
    ) -> Vec<PathBuf> {
        let mut raw_paths = Vec::new();

        if let Some(value) = tool_input {
            Self::collect_tool_paths(value, &mut raw_paths);
        }

        let mut normalized_paths = Vec::new();
        for raw in raw_paths {
            if let Some(path) = Self::normalize_hook_path(&raw, cwd) {
                let pb = PathBuf::from(&path);
                if !normalized_paths.contains(&pb) {
                    normalized_paths.push(pb);
                }
            }
        }

        normalized_paths
    }

    fn collect_tool_paths(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    let key_lower = key.to_ascii_lowercase();
                    let is_single_path_key = key_lower == "file_path"
                        || key_lower == "filepath"
                        || key_lower == "path"
                        || key_lower == "fspath";

                    let is_multi_path_key = key_lower == "files"
                        || key_lower == "filepaths"
                        || key_lower == "file_paths";

                    if is_single_path_key {
                        if let Some(path) = val.as_str() {
                            out.push(path.to_string());
                        }
                    } else if is_multi_path_key {
                        match val {
                            serde_json::Value::String(path) => out.push(path.to_string()),
                            serde_json::Value::Array(paths) => {
                                for path_value in paths {
                                    if let Some(path) = path_value.as_str() {
                                        out.push(path.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    Self::collect_tool_paths(val, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    Self::collect_tool_paths(item, out);
                }
            }
            serde_json::Value::String(s) => {
                if s.starts_with("file://") {
                    out.push(s.to_string());
                }
                super::parse::collect_apply_patch_paths_from_text(s, out);
            }
            _ => {}
        }
    }

    fn normalize_hook_path(raw_path: &str, cwd: &str) -> Option<String> {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return None;
        }

        let path_without_scheme = trimmed
            .strip_prefix("file://localhost")
            .or_else(|| trimmed.strip_prefix("file://"))
            .unwrap_or(trimmed);

        let path = Path::new(path_without_scheme);
        let joined = if path.is_absolute()
            || path_without_scheme.starts_with("\\\\")
            || path_without_scheme
                .as_bytes()
                .get(1)
                .map(|b| *b == b':')
                .unwrap_or(false)
        {
            PathBuf::from(path_without_scheme)
        } else {
            Path::new(cwd).join(path_without_scheme)
        };

        Some(joined.to_string_lossy().replace('\\', "/"))
    }

    fn storage_base() -> Option<PathBuf> {
        if let Ok(test_path) = std::env::var("GIT_AI_OPENCODE_STORAGE_PATH") {
            Some(PathBuf::from(test_path))
        } else {
            Self::opencode_data_path().ok()
        }
    }

    fn resolve_stream_source(session_id: &str) -> Option<(StreamSource, PathBuf)> {
        let opencode_path = Self::storage_base()?;

        // Try sqlite first
        let db_path = Self::resolve_sqlite_db_path(&opencode_path, session_id);
        if let Some(db_path) = db_path {
            let parent_id = Self::lookup_parent_session(&db_path, session_id);
            return Some((
                StreamSource {
                    path: db_path,
                    format: StreamFormat::OpenCodeSqlite,
                    session_id: generate_session_id(session_id, "opencode"),
                    external_session_id: session_id.to_string(),
                    external_parent_session_id: parent_id,
                },
                opencode_path,
            ));
        }

        None
    }

    fn lookup_parent_session(db_path: &Path, session_id: &str) -> Option<String> {
        let conn = crate::streams::agents::opencode::open_sqlite_readonly(db_path).ok()?;
        conn.query_row(
            "SELECT parent_id FROM session WHERE id = ?",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn opencode_data_path() -> Result<PathBuf, GitAiError> {
        #[cfg(target_os = "macos")]
        {
            let home = dirs::home_dir().ok_or_else(|| {
                GitAiError::Generic("Could not determine home directory".to_string())
            })?;
            Ok(home.join(".local").join("share").join("opencode"))
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
                Ok(PathBuf::from(xdg_data).join("opencode"))
            } else {
                let home = dirs::home_dir().ok_or_else(|| {
                    GitAiError::Generic("Could not determine home directory".to_string())
                })?;
                Ok(home.join(".local").join("share").join("opencode"))
            }
        }

        #[cfg(target_os = "windows")]
        {
            if let Ok(app_data) = std::env::var("APPDATA") {
                Ok(PathBuf::from(app_data).join("opencode"))
            } else if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
                Ok(PathBuf::from(local_app_data).join("opencode"))
            } else {
                Err(GitAiError::Generic(
                    "Neither APPDATA nor LOCALAPPDATA is set".to_string(),
                ))
            }
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(GitAiError::PresetError(
                "OpenCode storage path not supported on this platform".to_string(),
            ))
        }
    }

    fn resolve_sqlite_db_path(path: &Path, session_id: &str) -> Option<PathBuf> {
        let directory = if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "storage")
        {
            path.parent()?
        } else {
            path
        };
        let candidates = if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            std::fs::read_dir(directory)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        };
        let mut matched = None;
        for candidate in candidates {
            let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name != "opencode.db" && !(name.starts_with("opencode-") && name.ends_with(".db")) {
                continue;
            }
            let metadata = candidate.metadata().ok()?;
            if !metadata.is_file() || metadata.len() == 0 {
                continue;
            }
            let conn = crate::streams::agents::opencode::open_sqlite_readonly(&candidate).ok()?;
            conn.busy_timeout(std::time::Duration::from_millis(50))
                .ok()?;
            let has_sessions: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'session')",
                [], |row| row.get(0),
            ).ok()?;
            if !has_sessions {
                continue;
            }
            let found: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?)",
                    [session_id],
                    |row| row.get(0),
                )
                .ok()?;
            if found {
                // Copied sessions are ambiguous: do not attach a potentially stale transcript.
                if matched.is_some() {
                    return None;
                }
                matched = Some(candidate);
            }
        }
        matched
    }

    /// OpenCode V2 keeps sessions in `session_v2` and messages in `session_message`; it has no `session` or
    /// `message` table, so the V1 resolver skips such a database. The model that served a session is the
    /// `model.id` of its most recent assistant row. Only that id is read (`json_extract`), never message text,
    /// and the database is opened read-only. `known_db` is a database the V1 resolver already matched by
    /// session id (a database holding both table sets); otherwise every `opencode.db` / `opencode-*.db` in
    /// `base` is checked, and a session found in more than one of them, or an unreadable candidate, yields
    /// no model rather than a guess (the same rule the V1 resolver applies).
    fn resolve_v2_model(base: &Path, session_id: &str, known_db: Option<&Path>) -> Option<String> {
        const MODEL_QUERY: &str = "SELECT json_extract(data, '$.model.id') FROM session_message \
             WHERE session_id = ? AND type = 'assistant' AND json_extract(data, '$.model.id') IS NOT NULL \
             ORDER BY seq DESC LIMIT 1";
        let query_one = |db: &Path| -> Result<Option<String>, ()> {
            let conn = crate::streams::agents::opencode::open_sqlite_readonly(db).map_err(|_| ())?;
            conn.busy_timeout(std::time::Duration::from_millis(50))
                .map_err(|_| ())?;
            let has_v2: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'session_message')",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| ())?;
            if !has_v2 {
                return Ok(None);
            }
            let model = rusqlite::OptionalExtension::optional(conn.query_row(
                MODEL_QUERY,
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            ))
            .map_err(|_| ())?
            .flatten();
            Ok(model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()))
        };

        if let Some(db) = known_db {
            return query_one(db).ok().flatten();
        }

        let directory = if base
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "storage")
        {
            base.parent()?
        } else {
            base
        };
        let candidates: Vec<PathBuf> = if base.is_file() {
            vec![base.to_path_buf()]
        } else {
            std::fs::read_dir(directory)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        };
        let mut found: Option<String> = None;
        let mut hits = 0;
        for candidate in candidates {
            let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name != "opencode.db" && !(name.starts_with("opencode-") && name.ends_with(".db")) {
                continue;
            }
            let Ok(metadata) = candidate.metadata() else {
                return None;
            };
            if !metadata.is_file() || metadata.len() == 0 {
                continue;
            }
            match query_one(&candidate) {
                Err(()) => return None,
                Ok(None) => {}
                Ok(Some(model)) => {
                    hits += 1;
                    found = Some(model);
                }
            }
        }
        if hits == 1 { found } else { None }
    }

    /// V1 first (unchanged), then V2. `v1_db` is the database the V1 resolver matched, if any.
    fn lookup_model(session_id: &str, v1_db: Option<&Path>, base: Option<&Path>) -> Option<String> {
        let v1 = v1_db.and_then(|db| {
            crate::streams::model_extraction::extract_model(
                db,
                crate::streams::sweep::StreamFormat::OpenCodeSqlite,
                Some(session_id),
            )
            .ok()
            .flatten()
        });
        v1.or_else(|| Self::resolve_v2_model(base?, session_id, v1_db))
    }
}

impl AgentPreset for OpenCodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: OpenCodeHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let is_bash = hook_input
            .tool_name
            .as_deref()
            .map(|name| bash_tool::classify_tool(Agent::OpenCode, name) == ToolClass::Bash)
            .unwrap_or(false);

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let OpenCodeHookInput {
            hook_event_name: _,
            session_id,
            cwd,
            tool_input,
            tool_name: _,
            tool_use_id,
        } = hook_input;

        let file_paths = Self::extract_filepaths_from_tool_input(tool_input.as_ref(), &cwd);
        let bash_command = tool_input
            .as_ref()
            .and_then(|value| {
                value
                    .get("command")
                    .or_else(|| value.get("cmd"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        // Build metadata
        let mut metadata = HashMap::new();
        metadata.insert("session_id".to_string(), session_id.clone());
        if let Ok(test_path) = std::env::var("GIT_AI_OPENCODE_STORAGE_PATH") {
            metadata.insert("__test_storage_path".to_string(), test_path);
        }

        // Resolve transcript source
        let transcript_result = Self::resolve_stream_source(&session_id);

        let storage_base = Self::storage_base();
        let extracted_model = Self::lookup_model(
            &session_id,
            transcript_result.as_ref().map(|(ts, _)| ts.path.as_path()),
            storage_base.as_deref(),
        );

        let context = PresetContext {
            agent_id: AgentId {
                tool: "opencode".to_string(),
                id: session_id.clone(),
                model: extracted_model.unwrap_or_else(|| "unknown".to_string()),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
            metadata,
        };

        let stream_source = transcript_result.map(|(source, _)| source);

        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id_str),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
                stream_source,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source,
                tool_use_id: Some(tool_use_id_str),
            }),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    #[test]
    fn resolves_channel_database_by_session_not_filename_priority() {
        let dir = tempfile::tempdir().unwrap();
        for (name, session) in [("opencode.db", "old"), ("opencode-dev.db", "current")] {
            let conn = rusqlite::Connection::open(dir.path().join(name)).unwrap();
            conn.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY);")
                .unwrap();
            conn.execute("INSERT INTO session (id) VALUES (?)", [session])
                .unwrap();
        }
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(dir.path(), "current"),
            Some(dir.path().join("opencode-dev.db"))
        );
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(dir.path(), "missing"),
            None
        );
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(&dir.path().join("opencode-dev.db"), "current"),
            Some(dir.path().join("opencode-dev.db"))
        );
        let conn = rusqlite::Connection::open(dir.path().join("opencode.db")).unwrap();
        conn.execute("INSERT INTO session (id) VALUES (?)", ["current"])
            .unwrap();
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(dir.path(), "current"),
            None
        );
    }

    fn v2_db(dir: &Path, name: &str, rows: &[(&str, &str, i64, &str)]) -> PathBuf {
        // rows: (session_id, type, seq, data json)
        let path = dir.join(name);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, model TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        for (i, (sid, kind, seq, data)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO session_message VALUES (?, ?, ?, ?, 0, 0, ?)",
                rusqlite::params![format!("m{i}"), sid, kind, seq, data],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn v2_model_is_the_latest_assistant_row() {
        let dir = tempfile::tempdir().unwrap();
        v2_db(
            dir.path(),
            "opencode-v2.db",
            &[
                ("ses_a", "user", 1, r#"{"text":"SECRET PROMPT"}"#),
                ("ses_a", "assistant", 2, r#"{"model":{"id":"anthropic/claude-sonnet-5-5","providerID":"homelab"}}"#),
                ("ses_a", "assistant", 5, r#"{"model":{"id":"antigravity/antigravity-gemini-4-argon","providerID":"homelab"}}"#),
                ("ses_a", "user", 9, r#"{"model":{"id":"later-user-row-model"}}"#),
                ("ses_b", "assistant", 1, r#"{"model":{"id":"openai/gpt-5.6-sol","providerID":"homelab"}}"#),
            ],
        );
        assert_eq!(
            OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None).as_deref(),
            Some("antigravity/antigravity-gemini-4-argon")
        );
        assert_eq!(
            OpenCodePreset::resolve_v2_model(dir.path(), "ses_b", None).as_deref(),
            Some("openai/gpt-5.6-sol")
        );
    }

    #[test]
    fn v2_session_without_assistant_row_or_unknown_session_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        v2_db(
            dir.path(),
            "opencode-v2.db",
            &[
                ("ses_user_only", "user", 1, r#"{"text":"hi","model":{"id":"user-row-model"}}"#),
                ("ses_nomodel", "assistant", 1, r#"{"model":{}}"#),
                ("ses_blank", "assistant", 1, r#"{"model":{"id":"  "}}"#),
            ],
        );
        for sid in ["ses_user_only", "ses_nomodel", "ses_blank", "ses_missing"] {
            assert_eq!(OpenCodePreset::resolve_v2_model(dir.path(), sid, None), None, "{sid}");
        }
    }

    #[test]
    fn v2_lookup_on_empty_absent_or_non_sqlite_storage_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None), None);
        assert_eq!(
            OpenCodePreset::resolve_v2_model(&dir.path().join("does-not-exist"), "ses_a", None),
            None
        );
        std::fs::write(dir.path().join("opencode-empty.db"), "").unwrap();
        assert_eq!(OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None), None);
        std::fs::write(dir.path().join("opencode-bad.db"), "not a sqlite database").unwrap();
        // an unreadable candidate must not let a different database win by default
        v2_db(
            dir.path(),
            "opencode-v2.db",
            &[("ses_a", "assistant", 1, r#"{"model":{"id":"anthropic/x"}}"#)],
        );
        assert_eq!(OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None), None);
    }

    #[test]
    fn v2_session_present_in_two_databases_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        let row = [("ses_a", "assistant", 1, r#"{"model":{"id":"anthropic/x"}}"#)];
        v2_db(dir.path(), "opencode.db", &row);
        v2_db(dir.path(), "opencode-v2.db", &row);
        assert_eq!(OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None), None);
    }

    #[test]
    fn v2_lookup_ignores_v1_only_and_unrelated_databases() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = rusqlite::Connection::open(dir.path().join("opencode-dev.db")).unwrap();
        v1.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY); CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO session VALUES ('ses_a');",
        )
        .unwrap();
        v2_db(
            dir.path(),
            "opencode-v2.db",
            &[("ses_a", "assistant", 1, r#"{"model":{"id":"anthropic/claude-sonnet-5-5"}}"#)],
        );
        let other = rusqlite::Connection::open(dir.path().join("opencode-cache.db")).unwrap();
        other.execute_batch("CREATE TABLE cache (id TEXT);").unwrap();
        assert_eq!(
            OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None).as_deref(),
            Some("anthropic/claude-sonnet-5-5")
        );
    }

    #[test]
    fn v1_model_is_unchanged_and_wins_over_v2() {
        // V1-shaped database: the V1 table is used exactly as before.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode-dev.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO session VALUES ('ses_v1');
             INSERT INTO message VALUES ('m1', 'ses_v1', 1, 1, '{\"role\":\"assistant\",\"modelID\":\"claude-opus-4-6\",\"providerID\":\"anthropic\"}');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            OpenCodePreset::lookup_model("ses_v1", Some(&db), Some(dir.path())).as_deref(),
            Some("claude-opus-4-6")
        );
        // A database holding both table sets: V1 row present, so V1 answers; V2 is only the fallback.
        let both = dir.path().join("opencode.db");
        let c = rusqlite::Connection::open(&both).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO session VALUES ('ses_both'), ('ses_only_v2');
             INSERT INTO message VALUES ('m1', 'ses_both', 1, 1, '{\"role\":\"assistant\",\"modelID\":\"gpt-5\"}');
             INSERT INTO session_message VALUES ('s1', 'ses_both', 'assistant', 1, 0, 0, '{\"model\":{\"id\":\"v2-model\"}}');
             INSERT INTO session_message VALUES ('s2', 'ses_only_v2', 'assistant', 1, 0, 0, '{\"model\":{\"id\":\"v2-model\"}}');",
        )
        .unwrap();
        drop(c);
        assert_eq!(OpenCodePreset::lookup_model("ses_both", Some(&both), Some(dir.path())).as_deref(), Some("gpt-5"));
        assert_eq!(
            OpenCodePreset::lookup_model("ses_only_v2", Some(&both), Some(dir.path())).as_deref(),
            Some("v2-model")
        );
    }

    #[test]
    fn v2_database_is_opened_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = v2_db(
            dir.path(),
            "opencode-v2.db",
            &[("ses_a", "assistant", 1, r#"{"model":{"id":"anthropic/x"}}"#)],
        );
        let before = std::fs::read(&db).unwrap();
        assert_eq!(
            OpenCodePreset::resolve_v2_model(dir.path(), "ses_a", None).as_deref(),
            Some("anthropic/x")
        );
        assert_eq!(std::fs::read(&db).unwrap(), before, "the lookup must not modify the database");
    }

    fn make_opencode_input(event: &str, tool: &str) -> String {
        json!({
            "hook_event_name": event,
            "session_id": "oc-sess-123",
            "cwd": "/home/user/project",
            "tool_name": tool,
            "tool_use_id": "tu-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn unreadable_channel_prevents_stale_selection() {
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY); INSERT INTO session VALUES ('copied');",
        )
        .unwrap();
        std::fs::write(dir.path().join("opencode-dev.db"), "not a sqlite database").unwrap();
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(dir.path(), "copied"),
            None
        );
    }

    #[test]
    fn empty_and_non_session_databases_do_not_hide_valid_channel() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode-dev.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY); INSERT INTO session VALUES ('current');",
        )
        .unwrap();
        std::fs::write(dir.path().join("opencode-empty.db"), "").unwrap();
        let auxiliary = rusqlite::Connection::open(dir.path().join("opencode-cache.db")).unwrap();
        auxiliary
            .execute_batch("CREATE TABLE cache (id TEXT);")
            .unwrap();
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(dir.path(), "current"),
            Some(db.clone())
        );
        std::fs::create_dir(dir.path().join("storage")).unwrap();
        assert_eq!(
            OpenCodePreset::resolve_sqlite_db_path(&dir.path().join("storage"), "current"),
            Some(db)
        );
    }

    #[test]
    fn test_opencode_pre_file_edit() {
        let input = make_opencode_input("PreToolUse", "edit");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.context.external_session_id, "oc-sess-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert!(!e.file_paths.is_empty());
                assert_eq!(
                    e.context.metadata.get("session_id").map(String::as_str),
                    Some("oc-sess-123")
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_opencode_post_file_edit() {
        let input = make_opencode_input("PostToolUse", "write");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                // Transcript source depends on whether the storage path exists
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_pre_bash_call() {
        let input = make_opencode_input("PreToolUse", "bash");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_opencode_post_bash_call() {
        let input = make_opencode_input("PostToolUse", "shell");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_opencode_extracts_file_paths_from_tool_input() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_input": {
                "file_path": "src/main.rs",
                "fspath": "/project/src/lib.rs"
            }
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert!(!e.file_paths.is_empty());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_normalize_hook_path_absolute() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("/home/user/file.rs", "/project"),
            Some("/home/user/file.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_relative() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("src/main.rs", "/project"),
            Some("/project/src/main.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_file_uri() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("file:///home/user/file.rs", "/project"),
            Some("/home/user/file.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_empty() {
        assert_eq!(OpenCodePreset::normalize_hook_path("", "/project"), None);
    }

    #[test]
    fn test_opencode_collect_apply_patch_paths() {
        let mut out = Vec::new();
        super::super::parse::collect_apply_patch_paths_from_text(
            "*** Update File: src/main.rs\n*** Add File: src/new.rs",
            &mut out,
        );
        assert_eq!(out, vec!["src/main.rs", "src/new.rs"]);
    }

    #[test]
    fn test_opencode_extracts_file_paths_from_patch_key() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_input": {
                "patch": "*** Update File: /project/src/main.rs\n@@ old\n+new\n"
            }
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.file_paths, vec![PathBuf::from("/project/src/main.rs")]);
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_default_tool_use_id() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "bash"
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.tool_use_id, "bash");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }
}
