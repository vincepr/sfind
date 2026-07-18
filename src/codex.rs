use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    finish_session, text_blocks, timestamp_millis, Provider, ProviderDiscovery, Session,
    SessionHeader,
};

pub(crate) fn load(home: &Path) -> Result<ProviderDiscovery> {
    let root = home.join("sessions");
    if !root.exists() {
        return Ok(ProviderDiscovery::default());
    }
    let mut discovery = ProviderDiscovery::default();
    let titles = load_titles(&home.join("state_5.sqlite"), &mut discovery.warnings);
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery
                    .warnings
                    .push(format!("could not walk {}: {error}", root.display()));
                continue;
            }
        };
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            match parse(entry.path(), &mut discovery.warnings) {
                Ok(Some(mut session)) => {
                    if session.title.is_none() {
                        session.title = titles.get(&session.id).cloned();
                    }
                    discovery.sessions.push(session);
                }
                Ok(None) => {}
                Err(error) => discovery.warnings.push(format!("{error:#}")),
            }
        }
    }
    Ok(discovery)
}

fn load_titles(path: &Path, warnings: &mut Vec<String>) -> HashMap<String, String> {
    if !path.exists() {
        return HashMap::new();
    }
    let connection = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(error) => {
            warnings.push(format!(
                "could not open {} read-only: {error}",
                path.display()
            ));
            return HashMap::new();
        }
    };
    let mut statement = match connection.prepare("SELECT id, title FROM threads") {
        Ok(statement) => statement,
        Err(error) => {
            warnings.push(format!(
                "could not read titles from {}: {error}",
                path.display()
            ));
            return HashMap::new();
        }
    };
    let rows = match statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))) {
        Ok(rows) => rows,
        Err(error) => {
            warnings.push(format!(
                "could not query titles from {}: {error}",
                path.display()
            ));
            return HashMap::new();
        }
    };
    let mut titles = HashMap::new();
    let mut malformed_rows = 0;
    for row in rows {
        match row {
            Ok((id, title)) => {
                titles.insert(id, title);
            }
            Err(_) => malformed_rows += 1,
        }
    }
    if malformed_rows != 0 {
        warnings.push(format!(
            "ignored {malformed_rows} malformed title row(s) in {}",
            path.display()
        ));
    }
    titles
}

fn parse(path: &Path, warnings: &mut Vec<String>) -> Result<Option<Session>> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut id = None;
    let mut title = None;
    let mut directory = None;
    let mut updated_at = 0;
    let mut user_messages = Vec::new();
    let mut last_assistant_message = None;
    let mut fallback_users = Vec::new();
    let mut fallback_assistant_message = None;

    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("could not read {}", path.display()))?;
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!(
                    "could not parse {} line {}: {error}",
                    path.display(),
                    line_index + 1
                ));
                continue;
            }
        };
        updated_at = value
            .get("timestamp")
            .and_then(timestamp_millis)
            .unwrap_or(updated_at)
            .max(updated_at);
        let Some(payload) = value.get("payload") else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if id.is_none() {
                    id = payload.get("id").and_then(Value::as_str).map(str::to_owned);
                    directory = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                    title = payload
                        .get("session_name")
                        .or_else(|| payload.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some("response_item")
                if payload.get("type").and_then(Value::as_str) == Some("message") =>
            {
                let text = payload.get("content").and_then(|content| {
                    text_blocks(content, &["input_text", "output_text", "text"])
                });
                match (payload.get("role").and_then(Value::as_str), text) {
                    (Some("user"), Some(text)) if !is_injected_user_message(&text) => {
                        user_messages.push(text);
                    }
                    (Some("assistant"), Some(text)) => last_assistant_message = Some(text),
                    _ => {}
                }
            }
            Some("event_msg") => {
                let message = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .and_then(crate::normalized_text);
                match (payload.get("type").and_then(Value::as_str), message) {
                    (Some("user_message"), Some(text)) => fallback_users.push(text),
                    (Some("agent_message"), Some(text)) => fallback_assistant_message = Some(text),
                    _ => {}
                }
            }
            _ => {}
        };
    }
    if !fallback_users.is_empty() {
        user_messages = fallback_users;
    }
    if last_assistant_message.is_none() {
        last_assistant_message = fallback_assistant_message;
    }
    let id = id.or_else(|| id_from_filename(path));
    Ok(id.and_then(|id| {
        finish_session(
            SessionHeader {
                provider: Provider::Codex,
                id,
                title,
                directory,
                updated_at,
            },
            user_messages,
            last_assistant_message,
        )
    }))
}

fn is_injected_user_message(message: &str) -> bool {
    [
        "<environment_context>",
        "<user_instructions>",
        "<system-reminder>",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.get(stem.len().checked_sub(36)?..).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{id_from_filename, load};

    #[test]
    fn parses_codex_messages_and_ignores_tool_content() {
        let root = tempdir().expect("temp directory");
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).expect("sessions directory");
        let path = sessions.join("session.jsonl");
        fs::write(
            path,
            concat!(
                r#"{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{"id":"codex-1","cwd":"/work/app","session_name":"Auth cleanup"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:00:01Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/work/old","session_name":"Copied parent"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:01:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the login flow"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:01:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>private metadata</environment_context>"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:01:02Z","type":"event_msg","payload":{"type":"user_message","message":"Fix the login flow"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:02:00Z","type":"response_item","payload":{"type":"function_call","arguments":"secret tool text"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T10:03:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The login flow is fixed."}]}}"#,
                "\n"
            ),
        )
        .expect("fixture write");

        let discovery = load(root.path()).expect("load sessions");
        let sessions = discovery.sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "codex-1");
        assert_eq!(sessions[0].title.as_deref(), Some("Auth cleanup"));
        assert_eq!(sessions[0].user_messages, ["Fix the login flow"]);
        assert!(!sessions[0].search_text().contains("secret tool text"));
        assert!(!sessions[0].search_text().contains("private metadata"));
    }

    #[test]
    fn malformed_lines_warn_without_hiding_valid_session_data() {
        let root = tempdir().expect("temp directory");
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).expect("sessions directory");
        fs::write(
            sessions.join("session.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"codex-1"}}"#,
                "\nnot json\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Valid request"}}"#,
                "\n"
            ),
        )
        .expect("fixture write");

        let discovery = load(root.path()).expect("load sessions");

        assert_eq!(discovery.sessions.len(), 1);
        assert_eq!(discovery.sessions[0].user_messages, ["Valid request"]);
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].contains("line 2"));
        assert!(!discovery.warnings[0].contains("not json"));
    }

    #[test]
    fn non_ascii_filename_without_metadata_does_not_panic() {
        let filename = format!("{}a.jsonl", "é".repeat(18));

        assert_eq!(id_from_filename(Path::new(&filename)), None);
    }

    #[test]
    fn response_items_exclude_injected_environment_without_event_fallback() {
        let root = tempdir().expect("temp directory");
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).expect("sessions directory");
        fs::write(
            sessions.join("session.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"codex-1"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"<environment_context>private metadata</environment_context>"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"Real request"}}"#,
                "\n"
            ),
        )
        .expect("fixture write");

        let discovery = load(root.path()).expect("load sessions");

        assert_eq!(discovery.sessions[0].user_messages, ["Real request"]);
        assert!(!discovery.sessions[0]
            .search_text()
            .contains("private metadata"));
    }
}
