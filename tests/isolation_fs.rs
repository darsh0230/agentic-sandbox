use agentic_sandbox::common::paths::validate_relative_path;

#[test]
fn injected_paths_reject_escape_attempts() {
    assert!(validate_relative_path("input/task.md").is_ok());
    assert!(validate_relative_path("../secret").is_err());
    assert!(validate_relative_path("/etc/passwd").is_err());
    assert!(validate_relative_path("").is_err());
}
