use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{finish_session, normalized_text, Provider, Session};

#[derive(Debug)]
struct OpenCodeSession {
    id: String,
    title: String,
    directory: PathBuf,
    updated_at: i64,
    user_messages: Vec<String>,
    assistant_messages: Vec<String>,
}

pub(crate) fn load(path: &Path) -> Result<Vec<Session>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {} read-only", path.display()))?;
    let mut statement = connection
        .prepare(
            "SELECT s.id, s.title, s.directory, s.time_updated, m.data, p.data
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             LEFT JOIN part p ON p.message_id = m.id
             WHERE s.parent_id IS NULL
             ORDER BY s.time_updated DESC, s.id, m.time_created, m.id, p.time_created, p.id",
        )
        .context("unsupported OpenCode database schema")?;
    let mut rows = statement
        .query([])
        .context("could not query OpenCode sessions")?;
    let mut sessions = Vec::new();
    let mut current: Option<OpenCodeSession> = None;
    while let Some(row) = rows.next().context("could not read an OpenCode session")? {
        let id: String = row.get(0)?;
        if current.as_ref().is_some_and(|session| session.id != id) {
            push_finished(&mut sessions, current.take());
        }
        let session = current.get_or_insert_with(|| OpenCodeSession {
            id,
            title: row.get(1).unwrap_or_default(),
            directory: row
                .get::<_, String>(2)
                .map(PathBuf::from)
                .unwrap_or_default(),
            updated_at: row.get(3).unwrap_or_default(),
            user_messages: Vec::new(),
            assistant_messages: Vec::new(),
        });
        let message_data: Option<String> = row.get(4)?;
        let part_data: Option<String> = row.get(5)?;
        let (Some(message_data), Some(part_data)) = (message_data, part_data) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
            continue;
        };
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if part.get("synthetic").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .and_then(normalized_text)
        else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("user") => session.user_messages.push(text),
            Some("assistant") => session.assistant_messages.push(text),
            _ => {}
        }
    }
    push_finished(&mut sessions, current);
    Ok(sessions)
}

fn push_finished(sessions: &mut Vec<Session>, session: Option<OpenCodeSession>) {
    let Some(session) = session else {
        return;
    };
    if let Some(session) = finish_session(
        Provider::OpenCode,
        session.id,
        Some(session.title),
        Some(session.directory),
        session.updated_at,
        session.user_messages,
        session.assistant_messages,
    ) {
        sessions.push(session);
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    use super::load;

    #[test]
    fn loads_opencode_text_parts_in_message_order() {
        let root = tempdir().expect("temp directory");
        let path = root.path().join("opencode.db");
        let connection = Connection::open(&path).expect("database");
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_updated INTEGER,
                    parent_id TEXT
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY, message_id TEXT, time_created INTEGER, data TEXT
                );",
            )
            .expect("schema");
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4, NULL)",
                params!["ses_1", "API cleanup", "/work/api", 1000],
            )
            .expect("session");
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                params!["msg_1", "ses_1", 100, r#"{"role":"user"}"#],
            )
            .expect("message");
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4)",
                params![
                    "part_1",
                    "msg_1",
                    101,
                    r#"{"type":"text","text":"Fix API auth"}"#
                ],
            )
            .expect("part");
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4)",
                params![
                    "part_2",
                    "msg_1",
                    102,
                    r#"{"type":"tool","text":"hidden tool"}"#
                ],
            )
            .expect("tool part");
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4)",
                params![
                    "part_3",
                    "msg_1",
                    103,
                    r#"{"type":"text","text":"generated prompt","synthetic":true}"#
                ],
            )
            .expect("synthetic part");
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["ses_child", "Subagent", "/work/api", 2000, "ses_1"],
            )
            .expect("child session");
        drop(connection);

        let sessions = load(&path).expect("load sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_messages, ["Fix API auth"]);
        assert_eq!(sessions[0].title.as_deref(), Some("API cleanup"));
        assert!(!sessions[0].search_text().contains("hidden tool"));
        assert!(!sessions[0].search_text().contains("generated prompt"));
    }
}
