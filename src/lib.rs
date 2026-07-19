//! Discovery and normalization of local coding CLI sessions.

mod claude;
mod codex;
mod opencode;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};

/// A coding CLI that owns a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode CLI.
    OpenCode,
    /// Anthropic Claude Code CLI.
    Claude,
}

impl Provider {
    /// Returns the short provider label displayed by the finder.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Claude => "claude",
        }
    }
}

/// Provider-independent token counters for a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenUsage {
    /// Non-cached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Tokens written to a provider cache.
    pub cache_creation_tokens: u64,
    /// Tokens read from a provider cache.
    pub cache_read_tokens: u64,
}

impl TokenUsage {
    /// Returns input, output, cache creation, and cache read tokens combined.
    #[must_use]
    pub fn total_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cache_read_tokens)
    }

    fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
    }

    fn saturating_sub(self, previous: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_sub(previous.cache_creation_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_sub(previous.cache_read_tokens),
        }
    }

    fn has_decreased_from(self, previous: Self) -> bool {
        self.input_tokens < previous.input_tokens
            || self.output_tokens < previous.output_tokens
            || self.cache_creation_tokens < previous.cache_creation_tokens
            || self.cache_read_tokens < previous.cache_read_tokens
    }
}

fn add_usage(total: &mut Option<TokenUsage>, usage: TokenUsage) {
    match total {
        Some(total) => total.add_assign(usage),
        None => *total = Some(usage),
    }
}

/// Provider-independent session data used by the finder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// CLI that owns the session.
    pub provider: Provider,
    /// Provider session identifier used to resume it.
    pub id: String,
    /// Provider-generated title or summary, when available.
    pub title: Option<String>,
    /// Most recently recorded model identifier, when available.
    pub model: Option<String>,
    /// Most recently recorded reasoning-effort setting, when available.
    pub reasoning_effort: Option<String>,
    /// Working directory associated with the session.
    pub directory: Option<PathBuf>,
    /// Last activity time as Unix milliseconds.
    pub updated_at: i64,
    /// First user-authored message.
    pub first_user_message: String,
    /// Most recent user-authored message.
    pub last_user_message: String,
    /// Most recent assistant text response.
    pub last_assistant_message: Option<String>,
    /// All user-authored messages; this is the only message content searched.
    pub user_messages: Vec<String>,
    /// Token counters recorded by the provider, when available.
    pub usage: Option<TokenUsage>,
}

impl Session {
    /// Returns searchable text containing the directory, title, and user-authored messages.
    #[must_use]
    pub fn search_text(&self) -> String {
        let capacity = self.title.as_ref().map_or(0, String::len)
            + self
                .directory
                .as_ref()
                .map_or(0, |path| path.as_os_str().len())
            + self.user_messages.iter().map(String::len).sum::<usize>()
            + self.user_messages.len()
            + 2;
        let mut text = String::with_capacity(capacity);
        if let Some(title) = &self.title {
            text.push_str(title);
            text.push('\n');
        }
        if let Some(directory) = &self.directory {
            text.push_str(&directory.to_string_lossy());
            text.push('\n');
        }
        for message in &self.user_messages {
            text.push_str(message);
            text.push('\n');
        }
        text
    }
}

/// Default provider storage locations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRoots {
    /// Codex home containing `sessions/`.
    pub codex_home: PathBuf,
    /// OpenCode data directory containing `opencode.db`.
    pub opencode_data: PathBuf,
    /// Claude home containing `projects/`.
    pub claude_home: PathBuf,
}

impl SessionRoots {
    /// Resolves provider roots from environment overrides and standard home directories.
    ///
    /// # Errors
    ///
    /// Returns an error when the user's home or data directory cannot be resolved.
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().context("could not resolve the home directory")?;
        let data = dirs::data_dir().context("could not resolve the user data directory")?;
        Ok(Self {
            codex_home: env_path("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
            opencode_data: env_path("OPENCODE_DATA_DIR").unwrap_or_else(|| data.join("opencode")),
            claude_home: env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")),
        })
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Sessions found across all supported providers, plus non-fatal adapter warnings.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Sessions sorted by latest activity first.
    pub sessions: Vec<Session>,
    /// Provider errors and recoverable malformed-data warnings.
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct ProviderDiscovery {
    sessions: Vec<Session>,
    warnings: Vec<String>,
}

/// Loads all available provider sessions from the supplied roots.
#[must_use]
pub fn discover_sessions(roots: &SessionRoots) -> Discovery {
    let mut discovery = Discovery::default();
    collect(&mut discovery, "Codex", codex::load(&roots.codex_home));
    collect(
        &mut discovery,
        "OpenCode",
        opencode::load(&roots.opencode_data.join("opencode.db")),
    );
    collect(
        &mut discovery,
        "Claude Code",
        claude::load(&roots.claude_home.join("projects")),
    );
    discovery.sessions.sort_unstable_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.provider.label().cmp(right.provider.label()))
            .then_with(|| left.id.cmp(&right.id))
    });
    discovery
}

fn collect(discovery: &mut Discovery, provider: &str, result: Result<ProviderDiscovery>) {
    match result {
        Ok(mut provider_discovery) => {
            discovery.sessions.append(&mut provider_discovery.sessions);
            discovery.warnings.extend(
                provider_discovery
                    .warnings
                    .into_iter()
                    .map(|warning| format!("{provider}: {warning}")),
            );
        }
        Err(error) => discovery.warnings.push(format!("{provider}: {error:#}")),
    }
}

/// Starts the provider CLI and resumes the selected session.
///
/// # Errors
///
/// Returns an error when the provider process cannot be started.
pub fn resume_session(session: &Session) -> Result<ExitStatus> {
    session_process(session, false)
        .status()
        .with_context(|| format!("failed to start {}", session.provider.label()))
}

/// Starts the provider CLI with a new session forked from the selected session.
///
/// # Errors
///
/// Returns an error when the provider process cannot be started.
pub fn fork_session(session: &Session) -> Result<ExitStatus> {
    session_process(session, true)
        .status()
        .with_context(|| format!("failed to start {}", session.provider.label()))
}

/// Returns a Bash-compatible command that changes to the session directory and resumes it.
///
/// The returned command is text only and is not executed.
#[must_use]
pub fn session_command(session: &Session) -> String {
    printable_session_command(session, false)
}

/// Returns a Bash-compatible command that changes directory and forks the session.
///
/// The returned command is text only and is not executed.
#[must_use]
pub fn fork_session_command(session: &Session) -> String {
    printable_session_command(session, true)
}

fn printable_session_command(session: &Session, fork: bool) -> String {
    let resume = match (session.provider, fork) {
        (Provider::Codex, false) => format!("codex resume {}", shell_quote(&session.id)),
        (Provider::Codex, true) => format!("codex fork {}", shell_quote(&session.id)),
        (Provider::OpenCode, false) => {
            format!("opencode --session {}", shell_quote(&session.id))
        }
        (Provider::OpenCode, true) => {
            format!("opencode --session {} --fork", shell_quote(&session.id))
        }
        (Provider::Claude, false) => format!("claude --resume {}", shell_quote(&session.id)),
        (Provider::Claude, true) => {
            format!(
                "claude --resume {} --fork-session",
                shell_quote(&session.id)
            )
        }
    };
    match &session.directory {
        Some(directory) => format!("cd {} && {resume}", shell_quote_os(directory.as_os_str())),
        None => resume,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn shell_quote_os(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    if let Some(value) = value.to_str() {
        return shell_quote(value);
    }
    let mut quoted = String::from("$'");
    for &byte in value.as_bytes() {
        match byte {
            b'\\' => quoted.push_str("\\\\"),
            b'\'' => quoted.push_str("\\'"),
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => quoted.push_str(&format!("\\x{byte:02x}")),
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(not(unix))]
fn shell_quote_os(value: &OsStr) -> String {
    shell_quote(&value.to_string_lossy())
}

fn session_process(session: &Session, fork: bool) -> Command {
    let mut command = match session.provider {
        Provider::Codex => {
            let mut command = Command::new("codex");
            command
                .arg(if fork { "fork" } else { "resume" })
                .arg(&session.id);
            command
        }
        Provider::OpenCode => {
            let mut command = Command::new("opencode");
            command.arg("--session").arg(&session.id);
            if fork {
                command.arg("--fork");
            }
            if let Some(directory) = session.directory.as_ref().filter(|path| path.is_dir()) {
                command.arg(directory);
            }
            command
        }
        Provider::Claude => {
            let mut command = Command::new("claude");
            command.arg("--resume").arg(&session.id);
            if fork {
                command.arg("--fork-session");
            }
            command
        }
    };
    if session.provider != Provider::OpenCode {
        if let Some(directory) = session.directory.as_ref().filter(|path| path.is_dir()) {
            command.current_dir(directory);
        }
    }
    command
}

fn normalized_text(value: &str) -> Option<String> {
    let mut words = value.split_whitespace();
    let first = words.next()?;
    let mut text = String::with_capacity(value.len());
    text.push_str(first);
    for word in words {
        text.push(' ');
        text.push_str(word);
    }
    Some(text)
}

fn timestamp_millis(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|time| time.timestamp_millis())
        })
}

fn text_blocks(content: &serde_json::Value, accepted_types: &[&str]) -> Option<String> {
    if let Some(text) = content.as_str() {
        return normalized_text(text);
    }
    let mut text = String::new();
    for block in content.as_array()?.iter().filter(|block| {
        block
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| accepted_types.contains(&kind))
    }) {
        let Some(block_text) = block.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        for word in block_text.split_whitespace() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(word);
        }
    }
    (!text.is_empty()).then_some(text)
}

struct SessionHeader {
    provider: Provider,
    id: String,
    title: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    directory: Option<PathBuf>,
    updated_at: i64,
}

fn finish_session(
    header: SessionHeader,
    user_messages: Vec<String>,
    last_assistant_message: Option<String>,
    usage: Option<TokenUsage>,
) -> Option<Session> {
    Some(Session {
        provider: header.provider,
        id: header.id,
        title: header.title.and_then(|value| normalized_text(&value)),
        model: header.model,
        reasoning_effort: header.reasoning_effort,
        directory: header.directory,
        updated_at: header.updated_at,
        first_user_message: user_messages.first()?.clone(),
        last_user_message: user_messages.last()?.clone(),
        last_assistant_message,
        user_messages,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[cfg(unix)]
    use std::ffi::OsString;

    use super::{fork_session_command, session_command, session_process, Provider, Session};

    #[test]
    fn builds_each_providers_resume_command() {
        for (provider, program, arguments) in [
            (Provider::Codex, "codex", vec!["resume", "session-1"]),
            (
                Provider::OpenCode,
                "opencode",
                vec!["--session", "session-1", "."],
            ),
            (Provider::Claude, "claude", vec!["--resume", "session-1"]),
        ] {
            let session = Session {
                provider,
                id: "session-1".to_owned(),
                title: None,
                model: None,
                reasoning_effort: None,
                directory: Some(PathBuf::from(".")),
                updated_at: 0,
                first_user_message: "first".to_owned(),
                last_user_message: "last".to_owned(),
                last_assistant_message: None,
                user_messages: vec!["first".to_owned(), "last".to_owned()],
                usage: None,
            };

            let command = session_process(&session, false);

            assert_eq!(command.get_program(), program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), arguments);
        }
    }

    #[test]
    fn stale_directory_does_not_prevent_resume() {
        let session = Session {
            provider: Provider::Claude,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: Some(PathBuf::from("/path/that/does/not/exist")),
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned(), "last".to_owned()],
            usage: None,
        };

        let command = session_process(&session, false);

        assert!(command.get_current_dir().is_none());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--resume", "session-1"]
        );
    }

    #[test]
    fn printable_command_changes_directory_without_starting_provider() {
        let session = Session {
            provider: Provider::Codex,
            id: "session'1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: Some(PathBuf::from("/work/project's app")),
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: None,
        };

        let command = session_command(&session);

        assert_eq!(
            command,
            "cd '/work/project'\"'\"'s app' && codex resume 'session'\"'\"'1'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn printable_command_preserves_non_utf8_directory_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let session = Session {
            provider: Provider::Claude,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: Some(PathBuf::from(OsString::from_vec(
                b"/work/invalid-\xff".to_vec(),
            ))),
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: None,
        };

        assert_eq!(
            session_command(&session),
            "cd $'/work/invalid-\\xff' && claude --resume 'session-1'"
        );
    }

    #[test]
    fn builds_each_providers_fork_command() {
        for (provider, program, arguments) in [
            (Provider::Codex, "codex", vec!["fork", "session-1"]),
            (
                Provider::OpenCode,
                "opencode",
                vec!["--session", "session-1", "--fork"],
            ),
            (
                Provider::Claude,
                "claude",
                vec!["--resume", "session-1", "--fork-session"],
            ),
        ] {
            let session = Session {
                provider,
                id: "session-1".to_owned(),
                title: None,
                model: None,
                reasoning_effort: None,
                directory: None,
                updated_at: 0,
                first_user_message: "first".to_owned(),
                last_user_message: "last".to_owned(),
                last_assistant_message: None,
                user_messages: vec!["first".to_owned()],
                usage: None,
            };

            let command = session_process(&session, true);

            assert_eq!(command.get_program(), program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), arguments);
        }
    }

    #[test]
    fn printable_fork_command_uses_provider_fork_syntax() {
        let session = Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: None,
        };

        assert_eq!(fork_session_command(&session), "codex fork 'session-1'");
    }
}
