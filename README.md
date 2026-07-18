# session-search

Local fuzzy finder for Codex, OpenCode, and Claude Code sessions.

## Run

```bash
cargo run
```

Sessions are ordered by latest activity. Type to fuzzy-filter directories, provider titles,
summaries, and user-authored messages. Directory matches rank ahead of message-only matches. Select
with the arrow keys or mouse, inspect the first and latest sent
messages and latest received message, then press Enter to resume the session in its owning CLI.
On narrow terminals, the session list is placed above the details instead of beside it.

Use the clickable range control in the top-right to show all sessions, today, or the last 2, 3, 7,
or 30 local calendar days. `All` is selected by default.

Press `Ctrl-P` to close the finder and print a safely quoted command that changes to the project
directory and resumes the selected session. The command is printed but not executed. Press
`Ctrl-Delete` or `Ctrl-Backspace` to remove the last word from the search query.

Press `Tab` to toggle Fork mode. The selected row turns red and the details header shows `FORK`.
Enter then starts a new provider session forked from the selected history; `Ctrl-P` prints the
equivalent fork command instead. Continue mode remains the default.

Assistant messages, tool inputs and outputs, synthetic prompts, subagent sessions, and injected
environment metadata are excluded from search.

Use plain newest-first output without the TUI:

```bash
cargo run -- --list
```

## Install

```bash
cargo install --path .
session-search
```

Provider locations default to `$CODEX_HOME` or `~/.codex`, `$OPENCODE_DATA_DIR` or the operating
system data directory, and `$CLAUDE_CONFIG_DIR` or `~/.claude`. Use `--codex-home`,
`--opencode-data`, and `--claude-home` to override them.
