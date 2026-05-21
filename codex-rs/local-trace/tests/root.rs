use codex_local_trace::root::TraceRootInputs;
use codex_local_trace::root::TraceRootSource;
use codex_local_trace::root::git_ignore_warning;
use codex_local_trace::root::resolve_trace_root;

mod support;

#[test]
fn explicit_trace_dir_wins() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let explicit = temp.path().join("explicit");
    let workspace = temp.path().join("repo").join("nested");
    std::fs::create_dir_all(workspace.as_path())?;
    std::fs::create_dir_all(temp.path().join("repo").join(".git"))?;

    let resolution = resolve_trace_root(&TraceRootInputs {
        explicit_dir: Some(explicit.clone()),
        workspace_cwd: workspace,
        executable_repo_root: Some(temp.path().join("exec")),
        cwd: temp.path().join("cwd"),
    });

    assert_eq!(resolution.root, explicit);
    assert_eq!(resolution.source, TraceRootSource::Explicit);
    Ok(())
}

#[test]
fn workspace_git_root_is_default() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let repo = temp.path().join("repo");
    let nested = repo.join("a").join("b");
    std::fs::create_dir_all(repo.join(".git"))?;
    std::fs::create_dir_all(nested.as_path())?;

    let resolution = resolve_trace_root(&TraceRootInputs {
        explicit_dir: None,
        workspace_cwd: nested,
        executable_repo_root: None,
        cwd: temp.path().join("cwd"),
    });

    assert_eq!(resolution.root, repo.join("codex-traces"));
    assert_eq!(resolution.source, TraceRootSource::WorkspaceGit);
    Ok(())
}

#[test]
fn executable_repo_root_fallback_precedes_cwd_fallback() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let workspace = temp.path().join("workspace");
    let executable_repo_root = temp.path().join("codex");
    std::fs::create_dir_all(workspace.as_path())?;

    let resolution = resolve_trace_root(&TraceRootInputs {
        explicit_dir: None,
        workspace_cwd: workspace,
        executable_repo_root: Some(executable_repo_root.clone()),
        cwd: temp.path().join("cwd"),
    });

    assert_eq!(resolution.root, executable_repo_root.join("codex-traces"));
    assert_eq!(resolution.source, TraceRootSource::ExecutableRepoRoot);
    Ok(())
}

#[test]
fn warning_reports_whether_trace_root_is_ignored() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let repo = temp.path().join("repo");
    let trace_root = repo.join("codex-traces");
    std::fs::create_dir_all(repo.join(".git"))?;
    std::fs::write(repo.join(".gitignore"), "target/\ncodex-traces/\n")?;

    let warning = git_ignore_warning(&trace_root).expect("trace root is inside git");

    assert_eq!(warning.git_repo_root, repo);
    assert!(warning.ignored);
    Ok(())
}
