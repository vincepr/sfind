use assert_cmd::Command;

#[test]
fn help_describes_providers_and_plain_listing() {
    let mut command = Command::cargo_bin("sfind").unwrap();
    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Find and resume Codex, OpenCode, and Claude Code sessions",
        ))
        .stdout(predicates::str::contains("--list"))
        .stdout(predicates::str::contains("--codex-home"))
        .stdout(predicates::str::contains("--opencode-data"))
        .stdout(predicates::str::contains("--claude-home"));
}

#[test]
fn version_prints_package_version() {
    let mut command = Command::cargo_bin("sfind").unwrap();
    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("sfind 0.1.0"));
}
