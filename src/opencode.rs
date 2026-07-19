use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{
    add_usage, finish_session, normalized_text, Provider, ProviderDiscovery, Session,
    SessionHeader, TokenUsage,
};

#[derive(Debug)]
struct OpenCodeSession {
    id: String,
    title: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    directory: Option<PathBuf>,
    updated_at: i64,
    user_messages: Vec<String>,
    last_assistant_message: Option<String>,
    usage: Option<TokenUsage>,
    message_id: Option<String>,
    message_role: Option<String>,
}

pub(crate) fn load(path: &Path) -> Result<ProviderDiscovery> {
    if !path.exists() {
        return Ok(ProviderDiscovery::default());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {} read-only", path.display()))?;
    let mut statement = connection
        .prepare(
            "SELECT s.id, s.title, s.directory, s.time_updated, m.id, m.data, p.data
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
    let mut discovery = ProviderDiscovery::default();
    let mut current: Option<OpenCodeSession> = None;
    let mut malformed_records = 0_usize;
    while let Some(row) = rows.next().context("could not read an OpenCode session")? {
        let id: String = match row.get(0) {
            Ok(id) => id,
            Err(_) => {
                malformed_records += 1;
                continue;
            }
        };
        if current.as_ref().is_some_and(|session| session.id != id) {
            push_finished(&mut discovery.sessions, current.take());
        }
        if current.is_none() {
            let metadata = (
                row.get::<_, Option<String>>(1),
                row.get::<_, Option<String>>(2),
                row.get::<_, Option<i64>>(3),
            );
            let (Ok(title), Ok(directory), Ok(updated_at)) = metadata else {
                malformed_records += 1;
                continue;
            };
            current = Some(OpenCodeSession {
                id,
                title: title.unwrap_or_default(),
                model: None,
                reasoning_effort: None,
                directory: directory.map(PathBuf::from),
                updated_at: updated_at.unwrap_or_default(),
                user_messages: Vec::new(),
                last_assistant_message: None,
                usage: None,
                message_id: None,
                message_role: None,
            });
        }
        let Some(session) = current.as_mut() else {
            continue;
        };
        let (message_id, message_data): (Option<String>, Option<String>) =
            match (row.get(4), row.get(5)) {
                (Ok(message_id), Ok(message_data)) => (message_id, message_data),
                (Err(_), _) | (_, Err(_)) => {
                    malformed_records += 1;
                    continue;
                }
            };
        if session.message_id != message_id {
            session.message_id = message_id;
            session.message_role = None;
            if let Some(message_data) = message_data.as_deref() {
                match serde_json::from_str::<Value>(message_data) {
                    Ok(message) => {
                        if let Some(model) = message_model(&message) {
                            session.model = Some(model.to_owned());
                        }
                        if let Some(effort) = message_reasoning_effort(&message) {
                            session.reasoning_effort = Some(effort.to_owned());
                        }
                        session.message_role = message
                            .get("role")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if let Some(message_usage) = message_usage(&message) {
                            add_usage(&mut session.usage, message_usage);
                        }
                    }
                    Err(_) => malformed_records += 1,
                }
            }
        }
        let part_data: Option<String> = match row.get(6) {
            Ok(part_data) => part_data,
            Err(_) => {
                malformed_records += 1;
                continue;
            }
        };
        let Some(part_data) = part_data else {
            continue;
        };
        let part = match serde_json::from_str::<Value>(&part_data) {
            Ok(part) => part,
            Err(_) => {
                malformed_records += 1;
                continue;
            }
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
        match session.message_role.as_deref() {
            Some("user") => session.user_messages.push(text),
            Some("assistant") => session.last_assistant_message = Some(text),
            _ => {}
        }
    }
    push_finished(&mut discovery.sessions, current);
    if malformed_records != 0 {
        discovery.warnings.push(format!(
            "ignored {malformed_records} malformed database record(s) in {}",
            path.display()
        ));
    }
    Ok(discovery)
}

fn push_finished(sessions: &mut Vec<Session>, session: Option<OpenCodeSession>) {
    let Some(session) = session else {
        return;
    };
    if let Some(session) = finish_session(
        SessionHeader {
            provider: Provider::OpenCode,
            id: session.id,
            title: Some(session.title),
            model: session.model,
            reasoning_effort: session.reasoning_effort,
            directory: session.directory,
            updated_at: session.updated_at,
        },
        session.user_messages,
        session.last_assistant_message,
        session.usage,
    ) {
        sessions.push(session);
    }
}

fn message_model(message: &Value) -> Option<&str> {
    message
        .get("modelID")
        .or_else(|| message.get("model").and_then(|model| model.get("modelID")))
        .and_then(Value::as_str)
}

fn message_reasoning_effort(message: &Value) -> Option<&str> {
    message
        .get("variant")
        .or_else(|| message.get("model").and_then(|model| model.get("variant")))
        .and_then(Value::as_str)
}

fn message_usage(message: &Value) -> Option<TokenUsage> {
    let tokens = message.get("tokens")?;
    let cache = tokens.get("cache");
    Some(TokenUsage {
        input_tokens: token_count(tokens, "input"),
        output_tokens: token_count(tokens, "output")
            .saturating_add(token_count(tokens, "reasoning")),
        cache_creation_tokens: cache.map_or(0, |cache| token_count(cache, "write")),
        cache_read_tokens: cache.map_or(0, |cache| token_count(cache, "read")),
    })
}

fn token_count(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or_default()
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
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                params![
                    "msg_2",
                    "ses_1",
                    200,
                    r#"{"role":"assistant","modelID":"gpt-5.6-sol","variant":"high","tokens":{"input":100,"output":20,"reasoning":5,"cache":{"read":30,"write":10}}}"#
                ],
            )
            .expect("assistant message");
        for (id, created, text) in [
            ("part_4", 201, "First response part"),
            ("part_5", 202, "Final response part"),
        ] {
            connection
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id,
                        "msg_2",
                        created,
                        format!(r#"{{"type":"text","text":"{text}"}}"#)
                    ],
                )
                .expect("assistant part");
        }
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                params![
                    "msg_3",
                    "ses_1",
                    300,
                    r#"{"role":"user","model":{"modelID":"next-model","variant":"xhigh"}}"#
                ],
            )
            .expect("pending user message");
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["ses_child", "Subagent", "/work/api", 2000, "ses_1"],
            )
            .expect("child session");
        drop(connection);

        let sessions = load(&path).expect("load sessions").sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_messages, ["Fix API auth"]);
        assert_eq!(sessions[0].title.as_deref(), Some("API cleanup"));
        assert_eq!(sessions[0].model.as_deref(), Some("next-model"));
        assert_eq!(sessions[0].reasoning_effort.as_deref(), Some("xhigh"));
        let usage = sessions[0].usage.expect("OpenCode token usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.cache_creation_tokens, 10);
        assert_eq!(usage.cache_read_tokens, 30);
        assert!(!sessions[0].search_text().contains("hidden tool"));
        assert!(!sessions[0].search_text().contains("generated prompt"));
    }

    #[test]
    fn malformed_json_warns_without_hiding_valid_parts() {
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
                );
                INSERT INTO session VALUES ('ses_1', 'Title', '/work', 1000, NULL);
                INSERT INTO message VALUES ('msg_1', 'ses_1', 100, '{\"role\":\"user\"}');
                INSERT INTO part VALUES ('part_1', 'msg_1', 101, 'not json');
                 INSERT INTO part VALUES (
                     'part_2', 'msg_1', 102, '{\"type\":\"text\",\"text\":\"Valid request\"}'
                 );
                 INSERT INTO session VALUES ('ses_2', 'No directory', NULL, 2000, NULL);
                 INSERT INTO message VALUES ('msg_2', 'ses_2', 200, '{\"role\":\"user\"}');
                 INSERT INTO part VALUES (
                     'part_3', 'msg_2', 201, '{\"type\":\"text\",\"text\":\"Other request\"}'
                 );",
            )
            .expect("fixture database");
        drop(connection);

        let discovery = load(&path).expect("load sessions");

        assert_eq!(discovery.sessions.len(), 2);
        let valid = discovery
            .sessions
            .iter()
            .find(|session| session.id == "ses_1")
            .expect("valid session");
        assert_eq!(valid.user_messages, ["Valid request"]);
        let without_directory = discovery
            .sessions
            .iter()
            .find(|session| session.id == "ses_2")
            .expect("session without directory");
        assert_eq!(without_directory.directory, None);
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].contains("1 malformed database record"));
        assert!(!discovery.warnings[0].contains("not json"));
    }
}
