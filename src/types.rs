use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunRequest {
    pub run_id: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub injected_files: Vec<InjectedFile>,
    #[serde(default)]
    pub injected_readonly_dirs: Vec<InjectedDir>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub result_policy: ResultPolicy,
    #[serde(default)]
    pub cleanup_policy: CleanupPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedFile {
    pub relative_path: String,
    #[serde(with = "bytes_as_vec", alias = "content_utf8")]
    pub content: Vec<u8>,
    #[serde(default = "default_file_mode")]
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedDir {
    pub source_path: String,
    pub sandbox_path: String,
    #[serde(default = "default_true")]
    pub readonly: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_max_bytes: Option<u64>,
    pub pids_max: Option<u64>,
    pub cpu_quota: Option<CpuQuota>,
    pub stdout_max_bytes: Option<u64>,
    pub stderr_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuQuota {
    pub quota_us: u64,
    pub period_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Some(".pi/result.json".to_string())
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
