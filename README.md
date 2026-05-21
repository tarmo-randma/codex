# Personal Codex Fork

This repository is a personal fork of OpenAI's Codex CLI repository.

Upstream Codex is a local coding agent that runs on a developer machine and can inspect, edit, and test code in a workspace. This fork keeps that upstream base, while adding personal workflow changes that are useful for local experimentation and debugging.

For upstream project information, documentation, licensing, and installation guidance, see the original OpenAI Codex repository and official Codex documentation.

## Local Usage

The personal wrapper is available as:

```bash
my-codex
```

In this setup, `my-codex` runs the locally built debug binary from this checkout. Build it with:

```bash
cd codex-rs
cargo build -p codex-cli --bin codex
```

## Fork Changes

### Local Trace Recording

This fork adds local trace recording for Codex sessions when tracing is enabled.

The `my-codex` wrapper enables tracing by default with `CODEX_TRACE=1`. Traces are written under `codex-traces/` in the active workspace Git repository, or to `CODEX_TRACE_DIR` when that environment variable is set.

At a high level, traces record:

- session and turn metadata;
- model requests and responses, including streaming events and usage data;
- retry relationships between model requests;
- tool snapshots and tool-call request/result pairs;
- subagent parent/child trace links;
- internal/background requests such as compaction and memory-related model calls where wired.

Trace files are grouped for browsing. Model request artifacts live under per-request folders, and tool-call request/result files live under per-tool-call folders.

`codex-traces/` is ignored by this fork and should not be committed.
