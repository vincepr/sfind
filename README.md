# session-search

Local fuzzy finder for Codex, OpenCode, and Claude Code sessions.

## Run

```bash
cargo run
```

Sessions are ordered by latest activity. Type to fuzzy-filter provider titles, summaries, and
user-authored messages. Select with the arrow keys or mouse, inspect the first and latest sent
messages and latest received message, then press Enter to resume the session in its owning CLI.

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
