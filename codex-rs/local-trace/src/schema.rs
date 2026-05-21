use std::path::PathBuf;

use chrono::NaiveDateTime;
use serde::Deserialize;
use serde::Serialize;

pub const TRACE_FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub codex_session_id: Option<String>,
    pub provider_session_id: Option<String>,
    pub workspace_cwd: Option<PathBuf>,
    pub executable_repo_root: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub parent_session_path: Option<PathBuf>,
    pub parent_turn_id: Option<String>,
    pub parent_request_id: Option<String>,
    pub parent_spawn_id: Option<String>,
    pub parent_spawn_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub trace_format_version: u64,
    pub trace_session_path: PathBuf,
    pub created_at: NaiveDateTime,
    pub enabled: bool,
    pub codex_session_id: Option<String>,
    pub provider_session_id: Option<String>,
    pub workspace_cwd: Option<PathBuf>,
    pub parent_session_path: Option<PathBuf>,
    pub parent_turn_id: Option<String>,
    pub parent_request_id: Option<String>,
    pub parent_spawn_id: Option<String>,
    pub parent_spawn_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub trace_format_version: u64,
    pub created_at: NaiveDateTime,
    pub session: String,
    pub config: String,
    pub requests: String,
    pub tools: String,
    pub turns: Vec<String>,
    pub internal: Vec<String>,
    pub subagent_sessions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerScopeKind {
    UserTurn,
    Compaction,
    Background,
    Internal,
}

impl OwnerScopeKind {
    pub fn metadata_file(self) -> &'static str {
        match self {
            OwnerScopeKind::UserTurn => "turn.json",
            OwnerScopeKind::Compaction | OwnerScopeKind::Background | OwnerScopeKind::Internal => {
                "internal.json"
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub logical_request_id: Option<String>,
    pub retry_attempt: Option<u32>,
    pub previous_attempt_id: Option<String>,
    pub previous_attempt_path: Option<String>,
    pub owner_scope: Option<OwnerScopeKind>,
    pub owner_id: Option<String>,
    pub owner_path: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<NaiveDateTime>,
    pub ended_at: Option<NaiveDateTime>,
    pub status: Option<RequestStatus>,
    pub provider_request_id: Option<String>,
    pub provider_response_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Started,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub trace_request_id: String,
    pub logical_request_id: Option<String>,
    pub retry_attempt: Option<u32>,
    pub previous_attempt_id: Option<String>,
    pub previous_attempt_path: Option<String>,
    pub owner_scope: OwnerScopeKind,
    pub owner_id: Option<String>,
    pub owner_path: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub started_at: NaiveDateTime,
    pub ended_at: Option<NaiveDateTime>,
    pub status: RequestStatus,
    pub provider_request_id: Option<String>,
    pub provider_response_id: Option<String>,
    pub tool_snapshot_id: Option<String>,
    pub tool_snapshot_path: Option<String>,
    pub request_path: String,
    pub request_full_context_path: Option<String>,
    pub response_events_path: Option<String>,
    pub response_final_path: Option<String>,
    pub usage_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestUpdate {
    pub ended_at: Option<NaiveDateTime>,
    pub status: Option<RequestStatus>,
    pub provider_request_id: Option<String>,
    pub provider_response_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSnapshotRecord {
    pub id: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OwnerMetadata {
    pub codex_turn_id: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub trace_owner_id: String,
    pub owner_scope: OwnerScopeKind,
    pub codex_turn_id: Option<String>,
    pub label: Option<String>,
    pub prompt_slug: Option<String>,
    pub prompt_path: Option<String>,
    pub request_paths: Vec<String>,
    pub tool_call_paths: Vec<String>,
    pub status: Option<OwnerStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerStatus {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallMetadata {
    pub call_id: Option<String>,
    pub provider_tool_call_id: Option<String>,
    pub model_request_id: Option<String>,
    pub model_request_path: Option<String>,
    pub request_trace_id: Option<String>,
    pub request_trace_path: Option<String>,
    pub source: Option<serde_json::Value>,
    pub owner_id: Option<String>,
    pub owner_path: Option<String>,
    pub codex_turn_id: Option<String>,
    pub approval_policy: Option<String>,
    pub approval_required: Option<bool>,
    pub approval_outcome: Option<String>,
    pub sandbox: Option<serde_json::Value>,
    pub started_at: Option<NaiveDateTime>,
    pub ended_at: Option<NaiveDateTime>,
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentMetadata {
    pub subagent_id: Option<String>,
    pub name: Option<String>,
    pub parent_turn_id: Option<String>,
    pub nested_trace_path: Option<PathBuf>,
}
