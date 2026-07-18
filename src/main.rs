mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use session_search::{
    discover_sessions, fork_session, fork_session_command, resume_session, session_command,
    Session, SessionRoots,
};
use tracing::warn;

#[derive(Debug, Parser)]
#[command(
    name = "session-search",
    version,
    about = "Find and resume Codex, OpenCode, and Claude Code sessions"
)]
struct Cli {
    /// Print sessions newest-first instead of opening the interactive finder.
    #[arg(long)]
    list: bool,

    /// Override the Codex home directory.
    #[arg(long, value_name = "PATH")]
    codex_home: Option<PathBuf>,

    /// Override the OpenCode data directory.
    #[arg(long, value_name = "PATH")]
    opencode_data: Option<PathBuf>,

    /// Override the Claude Code configuration directory.
    #[arg(long, value_name = "PATH")]
    claude_home: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .compact()
        .init();
    let cli = Cli::parse();
    let mut roots = SessionRoots::discover()?;
    if let Some(path) = cli.codex_home {
        roots.codex_home = path;
    }
    if let Some(path) = cli.opencode_data {
        roots.opencode_data = path;
    }
    if let Some(path) = cli.claude_home {
        roots.claude_home = path;
    }
    let discovery = discover_sessions(&roots);
    for warning in &discovery.warnings {
        warn!("{warning}");
    }
    if cli.list {
        print_sessions(&discovery.sessions);
        return Ok(());
    }
    let Some(selection) = ui::pick(&discovery.sessions, discovery.warnings.len())? else {
        return Ok(());
    };
    let index = selection.index();
    let session = discovery
        .sessions
        .get(index)
        .context("selected session disappeared")?;
    let status = match selection {
        ui::Selection::Continue(_) => resume_session(session)?,
        ui::Selection::Fork(_) => fork_session(session)?,
        ui::Selection::PrintContinueCommand(_) => {
            println!("{}", session_command(session));
            return Ok(());
        }
        ui::Selection::PrintForkCommand(_) => {
            println!("{}", fork_session_command(session));
            return Ok(());
        }
    };
    if !status.success() {
        anyhow::bail!("{} exited with status {status}", session.provider.label());
    }
    Ok(())
}

fn print_sessions(sessions: &[Session]) {
    for session in sessions {
        let timestamp = DateTime::from_timestamp_millis(session.updated_at)
            .map(|time| {
                time.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown-time".to_owned());
        println!(
            "{}\t{}\t{}\t{}\t{}",
            timestamp,
            session.provider.label(),
            session.id,
            session.title.as_deref().unwrap_or("untitled"),
            one_line(&session.last_user_message, 120)
        );
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let mut value = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() > max_chars {
        value = value.chars().take(max_chars.saturating_sub(3)).collect();
        value.push_str("...");
    }
    value
}
