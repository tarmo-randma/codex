use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use chrono::Local;
use serde::Serialize;
use serde_json::json;

use crate::blob::BlobStore;
use crate::config::TraceConfig;
use crate::naming;
use crate::naming::TraceNamer;
use crate::owner_state::OwnerScope;
use crate::owner_state::activate_owner_scope;
use crate::owner_state::owner_scope_from_handle;
use crate::owner_state::update_owner_record;
use crate::owner_state::update_owner_scope_record;
use crate::owner_state::write_owner_record;
use crate::root;
use crate::schema::Manifest;
use crate::schema::OwnerMetadata;
use crate::schema::OwnerRecord;
use crate::schema::OwnerScopeKind;
use crate::schema::OwnerStatus;
use crate::schema::RequestMetadata;
use crate::schema::RequestRecord;
use crate::schema::RequestStatus;
use crate::schema::RequestUpdate;
use crate::schema::SessionMetadata;
use crate::schema::SessionRecord;
use crate::schema::SubagentMetadata;
use crate::schema::TRACE_FORMAT_VERSION;
use crate::schema::ToolCallMetadata;
use crate::schema::ToolSnapshotRecord;
use crate::writer;

#[derive(Debug, Clone)]
pub struct TraceRecorder {
    inner: Option<Arc<Mutex<TraceRecorderInner>>>,
}

#[derive(Debug)]
pub(super) struct TraceRecorderInner {
    pub(super) root: PathBuf,
    namer: TraceNamer,
    pub(super) current_owner: Option<OwnerScope>,
    pub(super) owner_stack: Vec<OwnerScope>,
    blob_store: BlobStore,
    manifest: Manifest,
    config_recorded: bool,
    request_index: Vec<RequestRecord>,
    tool_index: Vec<ToolSnapshotRecord>,
    current_tool_snapshot: Option<(String, TraceId)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceId {
    pub id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceOwner {
    pub id: String,
    pub path: String,
    pub(super) owner_scope: OwnerScopeKind,
    pub(super) metadata_file: String,
}

impl TraceRecorder {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn start_session(config: TraceConfig, metadata: SessionMetadata) -> Self {
        if !config.enabled() {
            return Self::disabled();
        }
        Self::start_session_at(config, metadata)
    }

    pub fn start_session_at(config: TraceConfig, metadata: SessionMetadata) -> Self {
        if !config.enabled() {
            return Self::disabled();
        }

        let cwd = metadata
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace_cwd = metadata
            .workspace_cwd
            .clone()
            .unwrap_or_else(|| cwd.clone());
        let resolution = root::resolve_trace_root(&root::TraceRootInputs {
            explicit_dir: config.trace_dir().cloned(),
            workspace_cwd,
            executable_repo_root: metadata.executable_repo_root.clone(),
            cwd,
        });
        let mut namer = TraceNamer::new();
        let Some(session_root) = allocate_session_root(&resolution.root, &mut namer) else {
            return Self::disabled();
        };
        let created_at = Local::now().naive_local();
        let manifest = Manifest {
            trace_format_version: TRACE_FORMAT_VERSION,
            created_at,
            session: "session.json".to_string(),
            config: "config.json".to_string(),
            requests: "requests/index.json".to_string(),
            tools: "tools/index.json".to_string(),
            turns: Vec::new(),
            internal: Vec::new(),
            subagent_sessions: Vec::new(),
        };
        let session = SessionRecord {
            trace_format_version: TRACE_FORMAT_VERSION,
            trace_session_path: session_root.clone(),
            created_at,
            enabled: true,
            codex_session_id: metadata.codex_session_id,
            provider_session_id: metadata.provider_session_id,
            workspace_cwd: metadata.workspace_cwd,
            parent_session_path: metadata.parent_session_path,
            parent_turn_id: metadata.parent_turn_id,
            parent_request_id: metadata.parent_request_id,
            parent_spawn_id: metadata.parent_spawn_id,
            parent_spawn_path: metadata.parent_spawn_path,
        };
        let result = (|| {
            writer::create_dir(&session_root.join("requests"))?;
            writer::create_dir(&session_root.join("tools"))?;
            writer::write_json_pretty(&session_root.join("manifest.json"), &manifest)?;
            writer::write_json_pretty(&session_root.join("session.json"), &session)?;
            writer::write_json_pretty(
                &session_root.join("config.json"),
                &json!({
                    "trace_format_version": TRACE_FORMAT_VERSION,
                    "config_recorded": false,
                }),
            )?;
            writer::write_json_pretty(
                &session_root.join("requests/index.json"),
                &Vec::<RequestRecord>::new(),
            )?;
            writer::write_json_pretty(
                &session_root.join("tools/index.json"),
                &Vec::<ToolSnapshotRecord>::new(),
            )?;
            Ok::<(), anyhow::Error>(())
        })();
        if result.is_err() {
            return Self::disabled();
        }
        Self {
            inner: Some(Arc::new(Mutex::new(TraceRecorderInner {
                root: session_root.clone(),
                namer,
                current_owner: None,
                owner_stack: Vec::new(),
                blob_store: BlobStore::new(&session_root),
                manifest,
                config_recorded: false,
                request_index: Vec::new(),
                tool_index: Vec::new(),
                current_tool_snapshot: None,
            }))),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn session_path(&self) -> Option<PathBuf> {
        self.with_inner(|inner| inner.root.clone())
    }

    pub fn record_config(&self, config: &impl Serialize) {
        self.with_inner_mut(|inner| {
            if !inner.config_recorded {
                let _ = writer::write_json_pretty(&inner.root.join("config.json"), config);
                inner.config_recorded = true;
            }
        });
    }

    pub fn start_turn(&self, prompt: &str, metadata: OwnerMetadata) -> Option<TraceId> {
        let owner = self.start_turn_scope(prompt, metadata)?;
        self.with_inner_mut(|inner| {
            activate_owner_scope(inner, &owner);
        });
        Some(TraceId {
            id: owner.id,
            path: owner.path,
        })
    }

    pub fn start_turn_scope(&self, prompt: &str, metadata: OwnerMetadata) -> Option<TraceOwner> {
        let prompt_slug = naming::turn_slug(prompt);
        self.start_owner_scope(
            "turns",
            OwnerScopeKind::UserTurn,
            &prompt_slug,
            OwnerRecord {
                trace_owner_id: String::new(),
                owner_scope: OwnerScopeKind::UserTurn,
                codex_turn_id: metadata.codex_turn_id,
                label: metadata.label,
                prompt_slug: Some(prompt_slug.clone()),
                prompt_path: Some("prompt.txt".to_string()),
                request_paths: Vec::new(),
                tool_call_paths: Vec::new(),
                status: None,
                error: None,
            },
        )
        .inspect(|id| {
            self.with_inner_mut(|inner| {
                let _ = writer::write_bytes(
                    &inner.root.join(&id.path).join("prompt.txt"),
                    prompt.as_bytes(),
                );
            });
        })
    }

    pub fn start_internal_call(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceId> {
        let owner = self.start_internal_call_scope(label, metadata)?;
        self.with_inner_mut(|inner| {
            activate_owner_scope(inner, &owner);
        });
        Some(TraceId {
            id: owner.id,
            path: owner.path,
        })
    }

    pub fn start_internal_call_scope(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceOwner> {
        let label = naming::internal_label(label.or(metadata.label.as_deref()));
        self.start_owner_scope(
            "internal",
            OwnerScopeKind::Internal,
            &label,
            OwnerRecord {
                trace_owner_id: String::new(),
                owner_scope: OwnerScopeKind::Internal,
                codex_turn_id: metadata.codex_turn_id,
                label: Some(label.clone()),
                prompt_slug: None,
                prompt_path: None,
                request_paths: Vec::new(),
                tool_call_paths: Vec::new(),
                status: None,
                error: None,
            },
        )
    }

    pub fn start_compaction_call(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceId> {
        let owner = self.start_compaction_call_scope(label, metadata)?;
        self.with_inner_mut(|inner| {
            activate_owner_scope(inner, &owner);
        });
        Some(TraceId {
            id: owner.id,
            path: owner.path,
        })
    }

    pub fn start_compaction_call_scope(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceOwner> {
        let label =
            naming::internal_label(label.or(metadata.label.as_deref()).or(Some("compaction")));
        self.start_owner_scope(
            "internal",
            OwnerScopeKind::Compaction,
            &label,
            OwnerRecord {
                trace_owner_id: String::new(),
                owner_scope: OwnerScopeKind::Compaction,
                codex_turn_id: metadata.codex_turn_id,
                label: Some(label.clone()),
                prompt_slug: None,
                prompt_path: None,
                request_paths: Vec::new(),
                tool_call_paths: Vec::new(),
                status: None,
                error: None,
            },
        )
    }

    pub fn start_background_call(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceId> {
        let owner = self.start_background_call_scope(label, metadata)?;
        self.with_inner_mut(|inner| {
            activate_owner_scope(inner, &owner);
        });
        Some(TraceId {
            id: owner.id,
            path: owner.path,
        })
    }

    pub fn start_background_call_scope(
        &self,
        label: Option<&str>,
        metadata: OwnerMetadata,
    ) -> Option<TraceOwner> {
        let label =
            naming::internal_label(label.or(metadata.label.as_deref()).or(Some("background")));
        self.start_owner_scope(
            "internal",
            OwnerScopeKind::Background,
            &label,
            OwnerRecord {
                trace_owner_id: String::new(),
                owner_scope: OwnerScopeKind::Background,
                codex_turn_id: metadata.codex_turn_id,
                label: Some(label.clone()),
                prompt_slug: None,
                prompt_path: None,
                request_paths: Vec::new(),
                tool_call_paths: Vec::new(),
                status: None,
                error: None,
            },
        )
    }

    pub fn record_model_request_for_owner(
        &self,
        owner: &TraceOwner,
        metadata: RequestMetadata,
        tool_snapshot: Option<&TraceId>,
        payload: &impl Serialize,
    ) -> Option<TraceId> {
        self.with_inner_mut(|inner| {
            let payload_value = serde_json::to_value(payload).ok()?;
            let request_dir = inner.root.join(&owner.path).join("requests");
            let request_relative_dir = format!("{}/requests", owner.path);
            let request_id = inner.namer.next("request");
            let request_path = format!("{request_relative_dir}/{request_id}/request.json");
            let record = RequestRecord {
                trace_request_id: request_id.clone(),
                logical_request_id: metadata.logical_request_id,
                retry_attempt: metadata.retry_attempt,
                previous_attempt_id: metadata.previous_attempt_id,
                previous_attempt_path: metadata.previous_attempt_path,
                owner_scope: metadata.owner_scope.unwrap_or(owner.owner_scope),
                owner_id: metadata.owner_id.or_else(|| Some(owner.id.clone())),
                owner_path: metadata.owner_path.or_else(|| Some(owner.path.clone())),
                provider: metadata.provider,
                endpoint: metadata.endpoint,
                model: metadata.model,
                started_at: metadata
                    .started_at
                    .unwrap_or_else(|| Local::now().naive_local()),
                ended_at: metadata.ended_at,
                status: metadata.status.unwrap_or(RequestStatus::Started),
                provider_request_id: metadata.provider_request_id,
                provider_response_id: metadata.provider_response_id,
                tool_snapshot_id: tool_snapshot.map(|snapshot| snapshot.id.clone()),
                tool_snapshot_path: tool_snapshot.map(|snapshot| snapshot.path.clone()),
                request_path: request_path.clone(),
                request_full_context_path: None,
                response_events_path: None,
                response_final_path: None,
                usage_path: None,
                error: metadata.error,
            };
            if writer::write_json_pretty(
                &request_dir.join(&request_id).join("request.json"),
                &payload_value,
            )
            .is_err()
            {
                return None;
            }
            if writer::write_json_pretty(&request_dir.join(&request_id).join("meta.json"), &record)
                .is_err()
            {
                return None;
            }
            let mut request_index = inner.request_index.clone();
            request_index.push(record);
            if writer::write_json_pretty(&inner.root.join("requests/index.json"), &request_index)
                .is_err()
            {
                return None;
            }
            inner.request_index = request_index;
            update_owner_scope_record(inner, owner, |record| {
                record.request_paths.push(request_path.clone());
            });
            if owner.owner_scope == OwnerScopeKind::UserTurn {
                activate_owner_scope(inner, owner);
            }
            Some(TraceId {
                id: request_id,
                path: request_path,
            })
        })?
    }

    pub fn record_model_request(
        &self,
        metadata: RequestMetadata,
        tool_snapshot: Option<&TraceId>,
        payload: &impl Serialize,
    ) -> Option<TraceId> {
        self.with_inner_mut(|inner| {
            let payload_value = serde_json::to_value(payload).ok()?;
            let owner = inner.current_owner.clone();
            let request_dir = owner
                .as_ref()
                .map(|owner| owner.dir.join("requests"))
                .unwrap_or_else(|| inner.root.join("requests"));
            let request_relative_dir = owner
                .as_ref()
                .map(|owner| format!("{}/requests", owner.relative_dir))
                .unwrap_or_else(|| "requests".to_string());
            let request_id = inner.namer.next("request");
            let request_path = format!("{request_relative_dir}/{request_id}/request.json");
            let owner_scope = metadata
                .owner_scope
                .or_else(|| owner.as_ref().map(|owner| owner.record.owner_scope))
                .unwrap_or(OwnerScopeKind::Background);
            let owner_id = metadata
                .owner_id
                .or_else(|| owner.as_ref().map(|owner| owner.id.clone()));
            let owner_path = metadata
                .owner_path
                .or_else(|| owner.as_ref().map(|owner| owner.relative_dir.clone()));
            let record = RequestRecord {
                trace_request_id: request_id.clone(),
                logical_request_id: metadata.logical_request_id,
                retry_attempt: metadata.retry_attempt,
                previous_attempt_id: metadata.previous_attempt_id,
                previous_attempt_path: metadata.previous_attempt_path,
                owner_scope,
                owner_id,
                owner_path,
                provider: metadata.provider,
                endpoint: metadata.endpoint,
                model: metadata.model,
                started_at: metadata
                    .started_at
                    .unwrap_or_else(|| Local::now().naive_local()),
                ended_at: metadata.ended_at,
                status: metadata.status.unwrap_or(RequestStatus::Started),
                provider_request_id: metadata.provider_request_id,
                provider_response_id: metadata.provider_response_id,
                tool_snapshot_id: tool_snapshot.map(|snapshot| snapshot.id.clone()),
                tool_snapshot_path: tool_snapshot.map(|snapshot| snapshot.path.clone()),
                request_path: request_path.clone(),
                request_full_context_path: None,
                response_events_path: None,
                response_final_path: None,
                usage_path: None,
                error: metadata.error,
            };
            if writer::write_json_pretty(
                &request_dir.join(&request_id).join("request.json"),
                &payload_value,
            )
            .is_err()
            {
                return None;
            }
            if writer::write_json_pretty(&request_dir.join(&request_id).join("meta.json"), &record)
                .is_err()
            {
                return None;
            }
            let mut request_index = inner.request_index.clone();
            request_index.push(record);
            if writer::write_json_pretty(&inner.root.join("requests/index.json"), &request_index)
                .is_err()
            {
                return None;
            }
            inner.request_index = request_index;
            if let Some(owner) = &mut inner.current_owner {
                owner.record.request_paths.push(request_path.clone());
                write_owner_record(&inner.root, owner);
            }
            Some(TraceId {
                id: request_id,
                path: request_path,
            })
        })?
    }

    pub fn record_request_full_context(&self, request: &TraceId, full_context: &impl Serialize) {
        if self.write_request_sidecar(request, "request.full_context.json", full_context, false) {
            self.update_request_record(request, |record| {
                record.request_full_context_path =
                    Some(request_artifact_path(request, "request.full_context.json"));
            });
        }
    }

    pub fn record_response_event(&self, request: &TraceId, event: &impl Serialize) {
        if self.write_request_sidecar(request, "response.events.jsonl", event, true) {
            self.update_request_record(request, |record| {
                record.response_events_path =
                    Some(request_artifact_path(request, "response.events.jsonl"));
            });
        }
    }

    pub fn record_model_response(&self, request: &TraceId, response: &impl Serialize) {
        if self.write_request_sidecar(request, "response.final.json", response, false) {
            self.update_request_record(request, |record| {
                record.response_final_path =
                    Some(request_artifact_path(request, "response.final.json"));
            });
        }
    }

    pub fn record_usage(&self, request: &TraceId, usage: &impl Serialize) {
        if self.write_request_sidecar(request, "usage.json", usage, false) {
            self.update_request_record(request, |record| {
                record.usage_path = Some(request_artifact_path(request, "usage.json"));
            });
        }
    }

    pub fn finish_model_request(&self, request: &TraceId, update: RequestUpdate) {
        self.update_request_record(request, |record| {
            record.ended_at = Some(
                update
                    .ended_at
                    .unwrap_or_else(|| Local::now().naive_local()),
            );
            if let Some(status) = update.status {
                record.status = status;
            }
            if let Some(provider_request_id) = update.provider_request_id {
                record.provider_request_id = Some(provider_request_id);
            }
            if let Some(provider_response_id) = update.provider_response_id {
                record.provider_response_id = Some(provider_response_id);
            }
            if let Some(error) = update.error {
                record.error = Some(error);
            }
        });
    }

    pub fn record_tool_snapshot(&self, reason: &str, tools: &impl Serialize) -> Option<TraceId> {
        self.with_inner_mut(|inner| {
            let tools_value = serde_json::to_value(tools).ok()?;
            let serialized = serde_json::to_string(&tools_value).unwrap_or_default();
            if let Some((current_serialized, id)) = &inner.current_tool_snapshot
                && *current_serialized == serialized
            {
                return Some(id.clone());
            }
            let id = inner.namer.next(&format!(
                "tools-{}",
                naming::sanitize_label(reason, "snapshot")
            ));
            let relative = format!("tools/{id}/tools.json");
            if writer::write_json_pretty(&inner.root.join(&relative), &tools_value).is_err() {
                return None;
            }
            let trace_id = TraceId {
                id: id.clone(),
                path: relative.clone(),
            };
            let mut tool_index = inner.tool_index.clone();
            tool_index.push(ToolSnapshotRecord {
                id,
                path: relative,
                reason: reason.to_string(),
            });
            if writer::write_json_pretty(&inner.root.join("tools/index.json"), &tool_index).is_err()
            {
                return None;
            }
            inner.tool_index = tool_index;
            inner.current_tool_snapshot = Some((serialized, trace_id.clone()));
            Some(trace_id)
        })?
    }

    pub fn record_tool_call_request(
        &self,
        tool_name: &str,
        metadata: ToolCallMetadata,
        arguments: &impl Serialize,
    ) -> Option<TraceId> {
        self.record_tool_call_file(None, tool_name, "request", metadata, arguments)
    }

    pub fn record_tool_call_request_for_owner(
        &self,
        owner: &TraceOwner,
        tool_name: &str,
        metadata: ToolCallMetadata,
        arguments: &impl Serialize,
    ) -> Option<TraceId> {
        self.record_tool_call_file(Some(owner), tool_name, "request", metadata, arguments)
    }

    pub fn record_tool_call_result(
        &self,
        tool_name: &str,
        metadata: ToolCallMetadata,
        result: &impl Serialize,
    ) -> Option<TraceId> {
        self.record_tool_call_file(None, tool_name, "result", metadata, result)
    }

    pub fn record_tool_call_result_for_owner(
        &self,
        owner: &TraceOwner,
        tool_name: &str,
        metadata: ToolCallMetadata,
        result: &impl Serialize,
    ) -> Option<TraceId> {
        self.record_tool_call_file(Some(owner), tool_name, "result", metadata, result)
    }

    pub fn record_subagent_spawn(
        &self,
        metadata: SubagentMetadata,
        input: &impl Serialize,
    ) -> Option<TraceId> {
        self.with_inner_mut(|inner| {
            let label = metadata.name.as_deref().unwrap_or("subagent");
            let id = inner
                .namer
                .next(&format!("{}.spawn", naming::tool_label(label)));
            let relative = format!("subagents/{id}.json");
            let value = json!({
                "metadata": metadata,
                "input": json_value(input),
            });
            let _ = writer::write_json_pretty(&inner.root.join(&relative), &value);
            Some(TraceId { id, path: relative })
        })?
    }

    pub fn record_subagent_nested_trace_path(&self, spawn: &TraceId, nested_trace_path: &Path) {
        self.with_inner(|inner| {
            let Some(relative) = relative_path(&inner.root, nested_trace_path) else {
                return;
            };
            let spawn_path = inner.root.join(&spawn.path);
            let Ok(mut value) = std::fs::read_to_string(&spawn_path)
                .ok()
                .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
                .ok_or(())
            else {
                return;
            };
            if let Some(metadata) = value.get_mut("metadata") {
                metadata["nested_trace_path"] = serde_json::Value::String(relative);
            }
            let _ = writer::write_json_pretty(&spawn_path, &value);
        });
    }

    pub fn start_subagent_session(
        &self,
        config: TraceConfig,
        metadata: SessionMetadata,
    ) -> TraceRecorder {
        let Some(parent_root) = self.session_path() else {
            return TraceRecorder::disabled();
        };
        let child_config = TraceConfig::from_env_map([
            (
                "CODEX_TRACE".to_string(),
                if config.enabled() { "1" } else { "0" }.to_string(),
            ),
            (
                "CODEX_TRACE_DIR".to_string(),
                parent_root
                    .join("subagent-sessions")
                    .to_string_lossy()
                    .to_string(),
            ),
        ]);
        let child = TraceRecorder::start_session_at(child_config, metadata);
        if let Some(child_path) = child.session_path() {
            self.with_inner_mut(|inner| {
                if let Some(relative) = relative_path(&inner.root, &child_path) {
                    inner.manifest.subagent_sessions.push(relative);
                    write_manifest(inner);
                }
            });
        }
        child
    }

    pub fn finish_owner(&self, status: OwnerStatus, error: Option<String>) {
        self.with_inner_mut(|inner| {
            if let Some(mut owner) = inner.current_owner.take() {
                owner.record.status = Some(status);
                owner.record.error = error;
                write_owner_record(&inner.root, &owner);
            }
            inner.current_owner = inner.owner_stack.pop();
        });
    }

    pub fn finish_owner_scope(
        &self,
        owner: &TraceOwner,
        status: OwnerStatus,
        error: Option<String>,
    ) {
        self.with_inner_mut(|inner| {
            if inner
                .current_owner
                .as_ref()
                .is_some_and(|active| active.relative_dir == owner.path)
            {
                if let Some(mut active) = inner.current_owner.take() {
                    active.record.status = Some(status);
                    active.record.error = error;
                    write_owner_record(&inner.root, &active);
                }
                inner.current_owner = inner.owner_stack.pop();
                return;
            }

            inner
                .owner_stack
                .retain(|stacked| stacked.relative_dir != owner.path);
            update_owner_record(&inner.root, owner, |record| {
                record.status = Some(status);
                record.error = error;
            });
        });
    }

    pub fn finish_session(&self, status: OwnerStatus, error: Option<String>) {
        self.write_root_json(
            "session-status.json",
            &json!({
                "status": status,
                "error": error,
            }),
        );
    }

    pub fn blob_store(&self) -> Option<BlobStore> {
        self.with_inner(|inner| inner.blob_store.clone())
    }

    fn start_owner_scope(
        &self,
        section: &str,
        owner_scope: OwnerScopeKind,
        label: &str,
        mut record: OwnerRecord,
    ) -> Option<TraceOwner> {
        self.with_inner_mut(|inner| {
            let id = inner.namer.next(label);
            let relative_dir = format!("{section}/{id}");
            let dir = inner.root.join(&relative_dir);
            let metadata_file = owner_scope.metadata_file().to_string();
            record.trace_owner_id = id.clone();
            let _ = writer::write_json_pretty(&dir.join(&metadata_file), &record);
            match owner_scope {
                OwnerScopeKind::UserTurn => inner.manifest.turns.push(relative_dir.clone()),
                OwnerScopeKind::Compaction
                | OwnerScopeKind::Background
                | OwnerScopeKind::Internal => inner.manifest.internal.push(relative_dir.clone()),
            }
            write_manifest(inner);
            Some(TraceOwner {
                id,
                path: relative_dir,
                owner_scope,
                metadata_file,
            })
        })?
    }

    fn record_tool_call_file(
        &self,
        explicit_owner: Option<&TraceOwner>,
        tool_name: &str,
        suffix: &str,
        mut metadata: ToolCallMetadata,
        payload: &impl Serialize,
    ) -> Option<TraceId> {
        self.with_inner_mut(|inner| {
            let owner = match explicit_owner {
                Some(owner) => owner_scope_from_handle(inner, owner)?,
                None => inner.current_owner.clone()?,
            };
            metadata.owner_id = metadata.owner_id.or_else(|| Some(owner.id.clone()));
            metadata.owner_path = metadata
                .owner_path
                .or_else(|| Some(owner.relative_dir.clone()));
            metadata.codex_turn_id = metadata
                .codex_turn_id
                .or_else(|| owner.record.codex_turn_id.clone());
            let tool_call_dir = if suffix == "result" {
                metadata.request_trace_path.as_ref().and_then(|path| {
                    path.strip_suffix("/request.json")
                        .map(std::string::ToString::to_string)
                })
            } else {
                None
            };
            let id = tool_call_dir
                .as_ref()
                .and_then(|dir| dir.rsplit('/').next().map(std::string::ToString::to_string))
                .unwrap_or_else(|| inner.namer.next(&naming::tool_label(tool_name)));
            let relative_dir =
                tool_call_dir.unwrap_or_else(|| format!("{}/tool-calls/{id}", owner.relative_dir));
            let relative = format!("{relative_dir}/{suffix}.json");
            let value = json!({
                "tool_name": tool_name,
                "metadata": metadata,
                "payload": json_value(payload),
            });
            if writer::write_json_pretty(&inner.root.join(&relative), &value).is_err() {
                return None;
            }
            if let Some(explicit_owner) = explicit_owner {
                update_owner_scope_record(inner, explicit_owner, |record| {
                    record.tool_call_paths.push(relative.clone());
                });
            } else if let Some(owner) = &mut inner.current_owner {
                owner.record.tool_call_paths.push(relative.clone());
                write_owner_record(&inner.root, owner);
            }
            Some(TraceId { id, path: relative })
        })?
    }

    fn write_request_sidecar(
        &self,
        request: &TraceId,
        artifact_name: &str,
        value: &impl Serialize,
        jsonl: bool,
    ) -> bool {
        self.with_inner(|inner| {
            let path = inner
                .root
                .join(request_artifact_path(request, artifact_name));
            if jsonl {
                writer::append_jsonl(&path, value).is_ok()
            } else {
                writer::write_json_pretty(&path, value).is_ok()
            }
        })
        .unwrap_or(false)
    }

    fn write_root_json(&self, relative: &str, value: &impl Serialize) {
        self.with_inner(|inner| {
            let _ = writer::write_json_pretty(&inner.root.join(relative), value);
        });
    }

    fn update_request_record(&self, request: &TraceId, update: impl FnOnce(&mut RequestRecord)) {
        self.with_inner_mut(|inner| {
            if let Some(record) = inner
                .request_index
                .iter_mut()
                .find(|record| record.trace_request_id == request.id)
            {
                update(record);
                write_request_meta(&inner.root, record);
                let _ = writer::write_json_pretty(
                    &inner.root.join("requests/index.json"),
                    &inner.request_index,
                );
            }
        });
    }

    fn with_inner<T>(&self, f: impl FnOnce(&TraceRecorderInner) -> T) -> Option<T> {
        let inner = self.inner.as_ref()?;
        let guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        Some(f(&guard))
    }

    fn with_inner_mut<T>(&self, f: impl FnOnce(&mut TraceRecorderInner) -> T) -> Option<T> {
        let inner = self.inner.as_ref()?;
        let mut guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        Some(f(&mut guard))
    }
}

fn write_manifest(inner: &TraceRecorderInner) {
    let _ = writer::write_json_pretty(&inner.root.join("manifest.json"), &inner.manifest);
}

fn allocate_session_root(root: &Path, namer: &mut TraceNamer) -> Option<PathBuf> {
    for _ in 0..1000 {
        let session_root = root.join(namer.next("session"));
        match writer::create_dir_new(&session_root) {
            Ok(()) => return Some(session_root),
            Err(_) if session_root.exists() => continue,
            Err(_) => return None,
        }
    }
    None
}

fn write_request_meta(root: &Path, record: &RequestRecord) {
    let request_path = root.join(&record.request_path);
    let meta_path = request_path.with_file_name("meta.json");
    let _ = writer::write_json_pretty(&meta_path, record);
}

fn request_artifact_path(request: &TraceId, artifact_name: &str) -> String {
    format!(
        "{}/{}",
        request.path.trim_end_matches("/request.json"),
        artifact_name
    )
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn json_value(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|err| {
        json!({
            "trace_serialization_error": err.to_string(),
        })
    })
}
