use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunRequest {
    pub run_id: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub injected_files: Vec<InjectedFile>,
    #[serde(default)]
    pub injected_readonly_dirs: Vec<InjectedDir>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub inject_headers: Vec<HeaderOverrideRule>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_true")]
    pub kill_on_disconnect: bool,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub result_policy: ResultPolicy,
    #[serde(default)]
    pub cleanup_policy: CleanupPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderOverrideRule {
    pub endpoint: String,
    #[serde(default)]
    pub override_header_key: Option<String>,
    #[serde(default)]
    pub override_header_value: Option<String>,
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default)]
    pub https_proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedFile {
    pub relative_path: String,
    #[serde(with = "bytes_as_vec", alias = "content_utf8")]
    pub content: Vec<u8>,
    #[serde(default = "default_file_mode")]
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedDir {
    pub source_path: String,
    pub sandbox_path: String,
    #[serde(default = "default_true")]
    pub readonly: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub memory_max_bytes: Option<u64>,
    pub pids_max: Option<u64>,
    pub cpu_quota: Option<CpuQuota>,
    pub stdout_max_bytes: Option<u64>,
    pub stderr_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuQuota {
    pub quota_us: u64,
    pub period_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultPolicy {
    #[serde(default)]
    pub collect_paths: Vec<String>,
    #[serde(default = "default_result_file")]
    pub result_file: Option<String>,
}

impl Default for ResultPolicy {
    fn default() -> Self {
        Self {
            collect_paths: Vec::new(),
            result_file: default_result_file(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum CleanupPolicy {
    #[default]
    DeleteWorkspaceAfterRun,
    PreserveWorkspaceForDebug,
    PreserveWorkspaceOnFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentRunEvent {
    WaitingForSlot {
        position: usize,
        max_waiting: usize,
    },
    Started {
        run_id: String,
        slot_id: String,
        pid: i32,
    },
    Stdout {
        data: Vec<u8>,
    },
    Stderr {
        data: Vec<u8>,
    },
    AgentEvent {
        json: Value,
    },
    ResultFileFound {
        relative_path: String,
        size: u64,
    },
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    ShuttingDown {
        signal: i32,
    },
    TimedOut,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub run_id: String,
    pub slot_id: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_tail: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub collected_files: Vec<CollectedFile>,
    pub result_json: Option<Value>,
    pub workspace_preserved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedFile {
    pub relative_path: String,
    pub content: Vec<u8>,
    pub size: u64,
}

fn default_timeout_ms() -> u64 {
    300_000
}
fn default_file_mode() -> u32 {
    0o600
}
fn default_true() -> bool {
    true
}
fn default_result_file() -> Option<String> {
    Some("result.json".to_string())
}

mod bytes_as_vec {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Vec<u8>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("bytes or UTF-8 string")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(v)
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(v.as_bytes().to_vec())
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(v.into_bytes())
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    out.push(byte);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{AgentRunEvent, AgentRunRequest, CleanupPolicy, InjectedFile, ResultPolicy};

    #[test]
    fn minimal_request_populates_every_default() {
        let request: AgentRunRequest =
            serde_json::from_str(r#"{"run_id":"r","command":["true"]}"#).unwrap();

        assert_eq!(request.run_id, "r");
        assert_eq!(request.command, ["true"]);
        assert!(request.injected_files.is_empty());
        assert!(request.injected_readonly_dirs.is_empty());
        assert!(request.env.is_empty());
        assert!(request.inject_headers.is_empty());
        assert_eq!(request.timeout_ms, 300_000);
        assert!(request.kill_on_disconnect);
        assert!(request.limits.memory_max_bytes.is_none());
        assert!(request.limits.pids_max.is_none());
        assert!(request.limits.cpu_quota.is_none());
        assert!(request.limits.stdout_max_bytes.is_none());
        assert!(request.limits.stderr_max_bytes.is_none());
        assert!(request.result_policy.collect_paths.is_empty());
        assert_eq!(
            request.result_policy.result_file.as_deref(),
            Some("result.json")
        );
        assert_eq!(
            request.cleanup_policy,
            CleanupPolicy::DeleteWorkspaceAfterRun
        );
    }

    #[test]
    fn nested_request_types_apply_their_defaults() {
        let request: AgentRunRequest = serde_json::from_value(json!({
            "run_id": "r",
            "command": ["true"],
            "injected_files": [{
                "relative_path": "input.txt",
                "content": [1, 2, 3]
            }],
            "injected_readonly_dirs": [{
                "source_path": "/fixtures",
                "sandbox_path": "/opt/fixtures"
            }],
            "inject_headers": [{
                "endpoint": "https://example.com/*"
            }]
        }))
        .unwrap();

        assert_eq!(request.injected_files[0].mode, 0o600);
        assert!(request.injected_readonly_dirs[0].readonly);
        let header = &request.inject_headers[0];
        assert!(header.override_header_key.is_none());
        assert!(header.override_header_value.is_none());
        assert!(header.http_proxy.is_none());
        assert!(header.https_proxy.is_none());
    }

    #[test]
    fn explicit_values_override_boolean_and_result_file_defaults() {
        let request: AgentRunRequest = serde_json::from_value(json!({
            "run_id": "r",
            "command": ["true"],
            "kill_on_disconnect": false,
            "result_policy": {
                "result_file": null
            }
        }))
        .unwrap();

        assert!(!request.kill_on_disconnect);
        assert_eq!(request.result_policy.result_file, None);
    }

    #[test]
    fn injected_content_accepts_text_alias_text_field_and_byte_arrays() {
        for (field, value, expected) in [
            ("content_utf8", json!("héllo"), "héllo".as_bytes().to_vec()),
            ("content", json!("text"), b"text".to_vec()),
            ("content", json!([0, 1, 127, 255]), vec![0, 1, 127, 255]),
            ("content", json!([]), Vec::new()),
        ] {
            let file: InjectedFile = serde_json::from_value(json!({
                "relative_path": "input.bin",
                field: value
            }))
            .unwrap();
            assert_eq!(file.content, expected, "{field}");
        }
    }

    #[test]
    fn injected_binary_content_round_trips_under_the_canonical_field() {
        let file = InjectedFile {
            relative_path: "input.bin".into(),
            content: vec![0, 128, 255],
            mode: 0o640,
        };

        let encoded = serde_json::to_value(&file).unwrap();
        assert_eq!(
            encoded,
            json!({
                "relative_path": "input.bin",
                "content": [0, 128, 255],
                "mode": 416
            })
        );
        let decoded: InjectedFile = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.relative_path, file.relative_path);
        assert_eq!(decoded.content, file.content);
        assert_eq!(decoded.mode, file.mode);
    }

    #[test]
    fn injected_content_rejects_invalid_bytes_missing_content_and_alias_collisions() {
        for value in [
            Value::Null,
            json!({}),
            json!([-1]),
            json!([256]),
            json!([1.5]),
            json!(["not-a-byte"]),
        ] {
            let result = serde_json::from_value::<InjectedFile>(json!({
                "relative_path": "input.bin",
                "content": value
            }));
            assert!(result.is_err());
        }

        assert!(
            serde_json::from_value::<InjectedFile>(json!({
                "relative_path": "input.bin"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<InjectedFile>(json!({
                "relative_path": "input.bin",
                "content": [1],
                "content_utf8": "duplicate"
            }))
            .is_err()
        );
    }

    #[test]
    fn cleanup_policy_has_stable_case_sensitive_wire_values() {
        for (policy, wire) in [
            (
                CleanupPolicy::DeleteWorkspaceAfterRun,
                "DeleteWorkspaceAfterRun",
            ),
            (
                CleanupPolicy::PreserveWorkspaceForDebug,
                "PreserveWorkspaceForDebug",
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                "PreserveWorkspaceOnFailure",
            ),
        ] {
            assert_eq!(serde_json::to_value(policy).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<CleanupPolicy>(json!(wire)).unwrap(),
                policy
            );
        }
        assert!(
            serde_json::from_value::<CleanupPolicy>(json!("preserve_workspace_for_debug")).is_err()
        );
    }

    #[test]
    fn run_events_have_stable_tagged_wire_formats() {
        let cases = vec![
            (
                AgentRunEvent::WaitingForSlot {
                    position: 2,
                    max_waiting: 10,
                },
                json!({"type":"waiting_for_slot","position":2,"max_waiting":10}),
            ),
            (
                AgentRunEvent::Started {
                    run_id: "r".into(),
                    slot_id: "slot-0001".into(),
                    pid: 42,
                },
                json!({"type":"started","run_id":"r","slot_id":"slot-0001","pid":42}),
            ),
            (
                AgentRunEvent::Stdout { data: vec![0, 255] },
                json!({"type":"stdout","data":[0,255]}),
            ),
            (
                AgentRunEvent::Stderr { data: vec![1, 2] },
                json!({"type":"stderr","data":[1,2]}),
            ),
            (
                AgentRunEvent::AgentEvent {
                    json: json!({"nested":[true,null]}),
                },
                json!({"type":"agent_event","json":{"nested":[true,null]}}),
            ),
            (
                AgentRunEvent::ResultFileFound {
                    relative_path: "out.bin".into(),
                    size: 9,
                },
                json!({"type":"result_file_found","relative_path":"out.bin","size":9}),
            ),
            (
                AgentRunEvent::Exited {
                    exit_code: None,
                    signal: Some(9),
                },
                json!({"type":"exited","exit_code":null,"signal":9}),
            ),
            (
                AgentRunEvent::ShuttingDown { signal: 15 },
                json!({"type":"shutting_down","signal":15}),
            ),
            (AgentRunEvent::TimedOut, json!({"type":"timed_out"})),
            (
                AgentRunEvent::Failed {
                    message: "boom".into(),
                },
                json!({"type":"failed","message":"boom"}),
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
            let decoded: AgentRunEvent = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
    }

    #[test]
    fn request_side_types_reject_unknown_fields_and_malformed_tuples() {
        for request in [
            json!({"run_id":"r","command":["true"],"timeuot_ms":1}),
            json!({
                "run_id":"r",
                "command":["true"],
                "limits":{"stdout_max_byte":1}
            }),
            json!({
                "run_id":"r",
                "command":["true"],
                "result_policy":{"result_files":"result.json"}
            }),
            json!({"run_id":"r","command":["true"],"env":["BAD"]}),
            json!({"run_id":"r","command":["true"],"env":[["ONLY_KEY"]]}),
            json!({"run_id":"r","command":["true"],"env":[["A","B","C"]]}),
        ] {
            assert!(
                serde_json::from_value::<AgentRunRequest>(request).is_err(),
                "malformed request was accepted"
            );
        }
    }

    #[test]
    fn unsigned_numeric_fields_reject_negative_and_overflow_values() {
        for request in [
            r#"{"run_id":"r","command":["true"],"timeout_ms":-1}"#,
            r#"{"run_id":"r","command":["true"],"timeout_ms":18446744073709551616}"#,
            r#"{"run_id":"r","command":["true"],"limits":{"pids_max":-1}}"#,
        ] {
            assert!(serde_json::from_str::<AgentRunRequest>(request).is_err());
        }
    }

    #[test]
    fn result_policy_default_matches_deserialized_default() {
        let direct = ResultPolicy::default();
        let deserialized: ResultPolicy = serde_json::from_value(json!({})).unwrap();

        assert_eq!(direct.collect_paths, deserialized.collect_paths);
        assert_eq!(direct.result_file, deserialized.result_file);
    }
}
