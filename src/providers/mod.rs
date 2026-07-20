mod claude;
mod codex;
mod opencode;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};

use crate::session::{Provider, Session};

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
    session_process(session, SessionAction::Resume)
        .status()
        .with_context(|| format!("failed to start {}", session.provider.label()))
}

/// Starts the provider CLI with a new session forked from the selected session.
///
/// # Errors
///
/// Returns an error when the provider process cannot be started.
pub fn fork_session(session: &Session) -> Result<ExitStatus> {
    session_process(session, SessionAction::Fork)
        .status()
        .with_context(|| format!("failed to start {}", session.provider.label()))
}

/// Returns a Bash-compatible command that changes to the session directory and resumes it.
///
/// The returned command is text only and is not executed.
#[must_use]
pub fn session_command(session: &Session) -> String {
    printable_session_command(session, SessionAction::Resume)
}

/// Returns a Bash-compatible command that changes directory and forks the session.
///
/// The returned command is text only and is not executed.
#[must_use]
pub fn fork_session_command(session: &Session) -> String {
    printable_session_command(session, SessionAction::Fork)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionAction {
    Resume,
    Fork,
}

fn session_process(session: &Session, action: SessionAction) -> Command {
    match session.provider {
        Provider::Codex => codex::session_process(session, action),
        Provider::OpenCode => opencode::session_process(session, action),
        Provider::Claude => claude::session_process(session, action),
    }
}

fn printable_session_command(session: &Session, action: SessionAction) -> String {
    let resume = match session.provider {
        Provider::Codex => codex::printable_session_command(session, action),
        Provider::OpenCode => opencode::printable_session_command(session, action),
        Provider::Claude => claude::printable_session_command(session, action),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[cfg(unix)]
    use std::ffi::OsString;

    use super::{fork_session_command, session_command, session_process, SessionAction};
    use crate::session::{Provider, Session};

    fn session(provider: Provider, directory: Option<PathBuf>) -> Session {
        Session {
            provider,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned(), "last".to_owned()],
            usage: None,
        }
    }

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
            let command = session_process(
                &session(provider, Some(PathBuf::from("."))),
                SessionAction::Resume,
            );

            assert_eq!(command.get_program(), program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), arguments);
        }
    }

    #[test]
    fn stale_directory_does_not_prevent_resume() {
        let session = session(
            Provider::Claude,
            Some(PathBuf::from("/path/that/does/not/exist")),
        );

        let command = session_process(&session, SessionAction::Resume);

        assert!(command.get_current_dir().is_none());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--resume", "session-1"]
        );
    }

    #[test]
    fn printable_command_changes_directory_without_starting_provider() {
        let mut session = session(Provider::Codex, Some(PathBuf::from("/work/project's app")));
        session.id = "session'1".to_owned();

        assert_eq!(
            session_command(&session),
            "cd '/work/project'\"'\"'s app' && codex resume 'session'\"'\"'1'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn printable_command_preserves_non_utf8_directory_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let session = session(
            Provider::Claude,
            Some(PathBuf::from(OsString::from_vec(
                b"/work/invalid-\xff".to_vec(),
            ))),
        );

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
            let command = session_process(&session(provider, None), SessionAction::Fork);

            assert_eq!(command.get_program(), program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), arguments);
        }
    }

    #[test]
    fn printable_fork_command_uses_provider_fork_syntax() {
        let session = session(Provider::Codex, None);

        assert_eq!(fork_session_command(&session), "codex fork 'session-1'");
    }
}
