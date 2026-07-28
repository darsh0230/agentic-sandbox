#[test]
fn request_accepts_content_utf8_alias() {
    let raw = r#"{
        "run_id":"smoke-001",
        "command":["bash","-lc","true"],
        "injected_files":[{"relative_path":"input/task.md","content_utf8":"hello","mode":420}]
    }"#;
    let req: agentic_sandbox::types::AgentRunRequest = serde_json::from_str(raw).unwrap();
    assert_eq!(req.injected_files[0].content, b"hello");
    assert!(req.kill_on_disconnect);
}
