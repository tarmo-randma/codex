use std::sync::Arc;

use codex_local_trace::TraceConfig;
use codex_local_trace::TraceRecorder;
use codex_local_trace::recorder::TraceId;
use codex_local_trace::root;
use codex_local_trace::schema::SessionMetadata;
use codex_local_trace::schema::SubagentMetadata;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SubAgentSource;
use serde::Serialize;
use serde_json::json;
use tracing::warn;

use super::SessionConfiguration;
use crate::config::Config;

#[cfg(test)]
tokio::task_local! {
    static TRACE_CONFIG_OVERRIDE: TraceConfig;
}

#[derive(Clone)]
pub(crate) struct LocalTraceParent {
    pub(crate) recorder: TraceRecorder,
    pub(crate) spawn: TraceId,
}

pub(crate) fn record_subagent_spawn(
    parent_recorder: &TraceRecorder,
    name: String,
    input: &impl Serialize,
) -> Option<LocalTraceParent> {
    if !parent_recorder.is_enabled() {
        return None;
    }
    let spawn = parent_recorder.record_subagent_spawn(
        SubagentMetadata {
            subagent_id: None,
            name: Some(name),
            parent_turn_id: None,
            nested_trace_path: None,
        },
        input,
    )?;
    Some(LocalTraceParent {
        recorder: parent_recorder.clone(),
        spawn,
    })
}

pub(crate) fn subagent_trace_name(source: &SubAgentSource) -> String {
    match source {
        SubAgentSource::ThreadSpawn {
            agent_nickname,
            agent_role,
            ..
        } => agent_nickname
            .clone()
            .or_else(|| agent_role.clone())
            .unwrap_or_else(|| "subagent".to_string()),
        SubAgentSource::Review => "review".to_string(),
        SubAgentSource::Compact => "compact".to_string(),
        SubAgentSource::MemoryConsolidation => "memory-consolidation".to_string(),
        SubAgentSource::Other(name) => name.clone(),
    }
}

pub(crate) fn start_session_recorder(
    config: &Arc<Config>,
    session_configuration: &SessionConfiguration,
    thread_id: ThreadId,
    installation_id: &str,
    parent: Option<LocalTraceParent>,
) -> TraceRecorder {
    let trace_config = trace_config();
    if !trace_config.enabled() {
        return TraceRecorder::disabled();
    }

    let parent_metadata = parent.as_ref().map(|parent| {
        (
            parent.recorder.session_path(),
            parent.spawn.id.clone(),
            parent.spawn.path.clone(),
        )
    });
    let metadata = SessionMetadata {
        codex_session_id: Some(thread_id.to_string()),
        provider_session_id: None,
        workspace_cwd: Some(config.cwd.to_path_buf()),
        executable_repo_root: std::env::current_exe()
            .ok()
            .and_then(|exe| root::find_git_root(&exe)),
        cwd: std::env::current_dir().ok(),
        parent_session_path: parent_metadata
            .as_ref()
            .and_then(|(session_path, _, _)| session_path.clone()),
        parent_turn_id: None,
        parent_request_id: None,
        parent_spawn_id: parent_metadata
            .as_ref()
            .map(|(_, spawn_id, _)| spawn_id.clone()),
        parent_spawn_path: parent_metadata
            .as_ref()
            .map(|(_, _, spawn_path)| spawn_path.clone()),
    };
    let recorder = if let Some(parent) = parent.as_ref() {
        let recorder = parent
            .recorder
            .start_subagent_session(trace_config, metadata);
        if let Some(child_path) = recorder.session_path() {
            parent
                .recorder
                .record_subagent_nested_trace_path(&parent.spawn, &child_path);
        }
        recorder
    } else {
        TraceRecorder::start_session(trace_config, metadata)
    };
    if let Some(session_path) = recorder.session_path() {
        eprintln!("Codex trace session: {}", session_path.display());
        let trace_root = session_path.parent().unwrap_or(session_path.as_path());
        if let Some(warning) = root::git_ignore_warning(trace_root)
            && !warning.ignored
        {
            warn!(
                trace_root = %warning.trace_root.display(),
                git_repo_root = %warning.git_repo_root.display(),
                "Codex trace root is inside a Git repository and is not ignored"
            );
            eprintln!(
                "warning: Codex trace root is inside a Git repository and is not ignored: {}",
                warning.trace_root.display()
            );
        }
    }
    recorder.record_config(&json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "installation_id": installation_id,
        "thread_id": thread_id.to_string(),
        "session_source": session_configuration.session_source.to_string(),
        "thread_source": session_configuration.thread_source.map(|source| format!("{source:?}")),
        "cwd": config.cwd.to_string_lossy(),
        "workspace_roots": config.workspace_roots.iter().map(|path| path.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "codex_home": config.codex_home.to_string_lossy(),
        "model": session_configuration.collaboration_mode.model(),
        "model_provider_id": config.model_provider_id.as_str(),
        "model_provider_name": config.model_provider.name.as_str(),
        "profile": config.active_profile.as_deref(),
        "approval_policy": session_configuration.approval_policy.value().to_string(),
        "approvals_reviewer": format!("{:?}", session_configuration.approvals_reviewer),
        "sandbox_policy": format!("{:?}", session_configuration.sandbox_policy()),
        "windows_sandbox_level": format!("{:?}", session_configuration.windows_sandbox_level),
        "service_tier": session_configuration.service_tier.as_deref(),
        "reasoning_effort": session_configuration.collaboration_mode.reasoning_effort(),
        "timezone": iana_time_zone::get_timezone().ok(),
    }));
    recorder
}

fn trace_config() -> TraceConfig {
    #[cfg(test)]
    {
        if let Ok(config) = TRACE_CONFIG_OVERRIDE.try_with(Clone::clone) {
            return config;
        }
    }

    TraceConfig::from_env()
}

#[cfg(test)]
pub(crate) async fn with_trace_config_for_tests<T>(
    trace_config: TraceConfig,
    future: std::pin::Pin<Box<dyn std::future::Future<Output = T> + '_>>,
) -> T {
    TRACE_CONFIG_OVERRIDE.scope(trace_config, future).await
}
