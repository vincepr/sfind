# sfind

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

Use the range button in the top-right to cycle through all sessions, today, or the last 3, 7, or
30 local calendar days. `All` is selected by default. The CLI button beside it cycles through all
providers, Codex, OpenCode (`open`), and Claude.

Press `Ctrl-P` to close the finder and print a safely quoted command that changes to the project
directory and resumes the selected session. The command is printed but not executed. Press
`Ctrl-Delete`, `Ctrl-Backspace`, or `Ctrl-W` to remove the last word from the search query. Use
`Ctrl-Up` and `Ctrl-Down` or the mouse wheel over the details pane to scroll long details.

Press `Tab` to cycle through Continue, Fork, Usage, and Graph views. In Fork mode, the selected row
turns red and the details header shows `FORK`. Enter then starts a new provider session forked from
the selected history; `Ctrl-P` prints the equivalent fork command instead. Continue mode remains
the default.

Usage summarizes the token counters for all sessions matching the current search, date, and CLI
filters, split by provider and model. OpenCode's recorded cost is shown when complete; Codex and
Claude Code do not record cost, so mixed or unavailable totals are marked unknown. No pricing data
is fetched. Graph groups each matching session's lifetime token total by its last activity date,
using day, week, month, quarter, or year buckets to fit the terminal. This is an activity
approximation rather than a per-request usage timeline.

Assistant messages, tool inputs and outputs, synthetic prompts, subagent sessions, and injected
environment metadata are excluded from search. Unreadable or malformed provider records are
skipped with warnings so other sessions can still be used.

Use plain newest-first output without the TUI:

```bash
cargo run -- --list
```

## Install

```bash
cargo install --path .
sfind
```

Provider locations default to `$CODEX_HOME` or `~/.codex`, `$OPENCODE_DATA_DIR` or the operating
system data directory, and `$CLAUDE_CONFIG_DIR` or `~/.claude`. Use `--codex-home`,
`--opencode-data`, and `--claude-home` to override them.
