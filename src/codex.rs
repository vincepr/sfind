use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{finish_session, text_blocks, timestamp_millis, Provider, Session};

pub(crate) fn load(home: &Path) -> Result<Vec<Session>> {
    let root = home.join("sessions");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let titles = load_titles(&home.join("state_5.sqlite"));
    let mut sessions = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry.with_context(|| format!("could not walk {}", root.display()))?;
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            if let Some(mut session) = parse(entry.path())? {
                if session.title.is_none() {
                    session.title = titles.get(&session.id).cloned();
                }
                sessions.push(session);
            }
        }
    }
    Ok(sessions)
}

fn load_titles(path: &Path) -> HashMap<String, String> {
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return HashMap::new();
    };
    let Ok(mut statement) = connection.prepare("SELECT id, title FROM threads") else {
        return HashMap::new();
    };
    let Ok(rows) = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))) else {
        return HashMap::new();
    };
    rows.filter_map(Result::ok).collect()
}

fn parse(path: &Path) -> Result<Option<Session>> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut id = None;
    let mut title = None;
    let mut directory = None;
    let mut updated_at = 0;
    let mut user_messages = Vec::new();
    let mut assistant_messages = Vec::new();
    let mut fallback_users = Vec::new();
    let mut fallback_assistants = Vec::new();

    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("could not read {}", path.display()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
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
                    (Some("user"), Some(text)) => user_messages.push(text),
                    (Some("assistant"), Some(text)) => assistant_messages.push(text),
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
                    (Some("agent_message"), Some(text)) => fallback_assistants.push(text),
                    _ => {}
                }
            }
            _ => {}
        };
    }
    if !fallback_users.is_empty() {
        user_messages = fallback_users;
    }
    if assistant_messages.is_empty() {
        assistant_messages = fallback_assistants;
    }
    let id = id.or_else(|| id_from_filename(path));
    Ok(id.and_then(|id| {
        finish_session(
            Provider::Codex,
            id,
            title,
            directory,
            updated_at,
            user_messages,
            assistant_messages,
        )
    }))
}

fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::load;

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

        let sessions = load(root.path()).expect("load sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "codex-1");
        assert_eq!(sessions[0].title.as_deref(), Some("Auth cleanup"));
        assert_eq!(sessions[0].user_messages, ["Fix the login flow"]);
        assert!(!sessions[0].search_text().contains("secret tool text"));
        assert!(!sessions[0].search_text().contains("private metadata"));
    }
}
