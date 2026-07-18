# Agent Guidelines for sfind

These rules apply to all contributors and coding agents working in this repository.

## Core Principles

- Keep the code minimal, explicit, and maintainable. Do not add temporary hacks,
  speculative abstractions, or compatibility code without a concrete need.
- Prefer the simplest design that correctly solves the current requirement.
- Optimize the discovery and filtering hot paths for runtime and memory without
  sacrificing clarity. Measure before adding parallelism, caching, or specialized
  dependencies.
- Avoid unnecessary allocations and repeated parsing. Prefer borrowing and iterators
  where they make the implementation clearer.
- Every behavior change and bug fix must be asserted by tests.
- Add dependencies only when they materially reduce correct, maintainable code and the
  standard library or an existing dependency cannot reasonably provide the behavior.

## Repository Invariants

- `sfind` discovers and resumes local Codex, OpenCode, and Claude Code sessions.
- Provider data is user-owned. Discovery must not modify transcripts, databases, or
  provider configuration.
- OpenCode SQLite access must remain read-only.
- A malformed session must not prevent valid sessions from other files or providers
  from being discovered. Preserve useful warning context for recoverable failures.
- Session ordering is newest-first unless a feature explicitly requires otherwise.
- Search covers directories, titles, summaries, and user-authored messages. Directory
  matches rank ahead of message-only matches.
- Exclude tool traffic, synthetic prompts, injected metadata, and subagent content from
  user-message search wherever the provider format makes that distinction possible.
- Date filters use local calendar-day boundaries, not rolling 24-hour periods.
- Continue and fork commands must preserve provider-specific syntax and safely quote
  shell-visible paths and identifiers.
- `Ctrl-P` prints a command but never executes it.
- The TUI must remain usable with keyboard and mouse on narrow and wide terminals.
  Always account for pane origins, borders, viewport offsets, and scrolling when
  calculating mouse hit locations.

## Rust Style

- Follow idiomatic Rust and the Rust API Guidelines.
- Use four spaces for indentation and let `rustfmt` manage formatting.
- Use `snake_case` for functions, variables, and modules; `PascalCase` for types and
  traits; and `SCREAMING_SNAKE_CASE` for constants.
- Use meaningful names and focused functions. Limit functions to five parameters;
  group cohesive inputs in a struct when that improves clarity.
- Prefer borrowing over ownership when possible and return early to reduce nesting.
- Prefer iterators and combinators over explicit loops when they are clearer.
- Use `enumerate()` instead of manual counters.
- Use exhaustive pattern matching. Avoid catch-all patterns when listing variants
  makes behavior safer.
- Derive `Debug`, `Clone`, `Copy`, `Eq`, and related traits where they are meaningful.
- Keep fields private by default.
- Do not use `unsafe` unless no safe design can satisfy the requirement. Document every
  safety invariant if `unsafe` is unavoidable.
- Do not use wildcard imports outside preludes and tightly scoped test modules.
- Do not add emoji or decorative Unicode. Unicode is appropriate in tests that verify
  terminal width, truncation, or provider input behavior.

## Comments and Documentation

- Document every public function, struct, enum, and method.
- Document errors for public fallible operations and include examples for APIs whose
  use is not evident from the signature.
- Add comments only for non-obvious intent, invariants, format quirks, or Rust-specific
  behavior. Do not restate the code or preserve the wording of a task request.
- Keep README behavior and command examples synchronized with the implementation.

## Error Handling

- Never use `.unwrap()` in production code paths.
- Use `.expect()` only for genuine invariants and include a descriptive message.
- Return `Result` for fallible operations and propagate errors with `?`.
- Use `anyhow` with `.context()` at application and I/O boundaries so errors identify
  the provider, file, database, or process operation that failed.
- Introduce a custom error type only when callers need to distinguish error variants;
  do not add `thiserror` solely to wrap errors that `anyhow` already handles well.
- Use `tracing` for diagnostics. Do not leave debug `println!` calls or `dbg!` macros.

## Performance and Memory

- Avoid unnecessary `String` creation and cloning. Prefer `&str` and `Cow<'_, str>`
  when ownership is conditional and the added complexity is justified.
- Use `Vec::with_capacity()` when a useful size estimate is already known.
- Keep list rendering virtualized; do not construct widgets for sessions outside the
  visible viewport.
- Do not read entire provider stores repeatedly when a bounded or incremental approach
  is practical.
- Add benchmarks only for a concrete performance question. Run benchmarks serially,
  without custom `RUSTFLAGS`, and never alter benchmark semantics to improve results.

## Testing

- Add or update tests for every feature, behavior change, and bug fix.
- Add regression tests that fail before a bug fix and pass afterward.
- Keep private-helper tests in source `#[cfg(test)]` modules. Put public crate and CLI
  behavior in `tests/`.
- Use temporary directories and fixture databases for filesystem behavior. Tests must
  not depend on or modify real provider data under the developer's home directory.
- Keep tests deterministic and independent of execution order, current time, locale,
  and terminal dimensions unless those inputs are explicitly controlled.
- Follow Arrange-Act-Assert where it improves readability.
- Do not commit ignored, commented-out, or flaky tests.

## Dependencies and Security

- Use `cargo` for package and dependency management.
- Specify dependency versions in `Cargo.toml` and commit corresponding `Cargo.lock`
  changes.
- Never store credentials, tokens, personal transcript content, or machine-specific
  provider data in the repository.
- Never log complete session content or sensitive provider metadata as diagnostics.
- Safely quote all generated shell commands. Do not interpolate untrusted values into
  shell syntax without escaping.

## Version Control

- Write concise, descriptive commit messages.
- Never commit credentials, generated debug artifacts, commented-out code, `dbg!`, or
  temporary diagnostics.
- Do not amend, force-push, tag, release, or open a pull request unless explicitly
  requested.
- Pull requests must include Summary, Changes, Verification, and Risks/Notes sections.

## Required Verification

Run all of the following before handing off Rust changes:

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo doc --no-deps
```

Before committing, also confirm:

- Public items have current doc comments.
- README examples match the current `sfind` CLI.
- No provider data, credentials, debug statements, or commented-out code were added.
- `git diff --check` reports no whitespace errors.

Prioritize clarity, correctness, and maintainability over cleverness.
