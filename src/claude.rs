use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    finish_session, text_blocks, timestamp_millis, Provider, ProviderDiscovery, Session,
    SessionHeader,
};

pub(crate) fn load(root: &Path) -> Result<ProviderDiscovery> {
    if !root.exists() {
        return Ok(ProviderDiscovery::default());
    }
    let mut discovery = ProviderDiscovery::default();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery
                    .warnings
                    .push(format!("could not walk {}: {error}", root.display()));
                continue;
            }
        };
        if entry.file_type().is_file()
            && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            && !entry
                .path()
                .components()
                .any(|part| part.as_os_str() == "subagents")
        {
            match parse(entry.path(), &mut discovery.warnings) {
                Ok(Some(session)) => discovery.sessions.push(session),
                Ok(None) => {}
                Err(error) => discovery.warnings.push(format!("{error:#}")),
            }
        }
    }
    Ok(discovery)
}

fn parse(path: &Path, warnings: &mut Vec<String>) -> Result<Option<Session>> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned);
    let mut title = None;
    let mut directory = None;
    let mut updated_at = 0;
    let mut user_messages = Vec::new();
    let mut last_assistant_message = None;

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
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        updated_at = value
            .get("timestamp")
            .and_then(timestamp_millis)
            .unwrap_or(updated_at)
            .max(updated_at);
        directory =
            directory.or_else(|| value.get("cwd").and_then(Value::as_str).map(PathBuf::from));
        if value.get("type").and_then(Value::as_str) == Some("summary") {
            title = value
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_owned);
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let text = message
            .get("content")
            .and_then(|content| text_blocks(content, &["text"]));
        match (message.get("role").and_then(Value::as_str), text) {
            (Some("user"), Some(text)) => user_messages.push(text),
            (Some("assistant"), Some(text)) => last_assistant_message = Some(text),
            _ => {}
        }
    }
    Ok(id.and_then(|id| {
        finish_session(
            SessionHeader {
                provider: Provider::Claude,
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::load;

    #[test]
    fn parses_claude_summary_and_only_human_text() {
        let root = tempdir().expect("temp directory");
        let project = root.path().join("project");
        fs::create_dir(&project).expect("project directory");
        fs::write(
            project.join("claude-1.jsonl"),
            concat!(
                r#"{"type":"summary","summary":"Database migration","sessionId":"claude-1","timestamp":"2026-01-02T10:00:00Z","cwd":"/work/db"}"#,
                "\n",
                r#"{"type":"user","sessionId":"claude-1","timestamp":"2026-01-02T10:01:00Z","message":{"role":"user","content":"Plan the migration"}}"#,
                "\n",
                r#"{"type":"user","sessionId":"claude-1","timestamp":"2026-01-02T10:02:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"private tool result"}]}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"claude-1","timestamp":"2026-01-02T10:03:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Here is the plan."}]}}"#,
                "\nnot json",
                "\n"
            ),
        )
        .expect("fixture write");

        let discovery = load(root.path()).expect("load sessions");
        let sessions = discovery.sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("Database migration"));
        assert_eq!(sessions[0].user_messages, ["Plan the migration"]);
        assert!(!sessions[0].search_text().contains("private tool result"));
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].contains("line 5"));
    }
}
