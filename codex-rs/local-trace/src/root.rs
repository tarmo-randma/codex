use std::fs;
use std::path::Path;
use std::path::PathBuf;

pub const TRACE_DIR_NAME: &str = "codex-traces";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRootInputs {
    pub explicit_dir: Option<PathBuf>,
    pub workspace_cwd: PathBuf,
    pub executable_repo_root: Option<PathBuf>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRootResolution {
    pub root: PathBuf,
    pub source: TraceRootSource,
    pub git_repo_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRootSource {
    Explicit,
    WorkspaceGit,
    ExecutableRepoRoot,
    CurrentWorkingDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIgnoreWarning {
    pub trace_root: PathBuf,
    pub git_repo_root: PathBuf,
    pub ignored: bool,
}

pub fn resolve_trace_root(inputs: &TraceRootInputs) -> TraceRootResolution {
    if let Some(root) = &inputs.explicit_dir {
        return TraceRootResolution {
            root: root.clone(),
            source: TraceRootSource::Explicit,
            git_repo_root: find_git_root(root),
        };
    }
    if let Some(repo_root) = find_git_root(&inputs.workspace_cwd) {
        return TraceRootResolution {
            root: repo_root.join(TRACE_DIR_NAME),
            source: TraceRootSource::WorkspaceGit,
            git_repo_root: Some(repo_root),
        };
    }
    if let Some(repo_root) = &inputs.executable_repo_root {
        return TraceRootResolution {
            root: repo_root.join(TRACE_DIR_NAME),
            source: TraceRootSource::ExecutableRepoRoot,
            git_repo_root: find_git_root(repo_root).or_else(|| Some(repo_root.clone())),
        };
    }
    TraceRootResolution {
        root: inputs.cwd.join(TRACE_DIR_NAME),
        source: TraceRootSource::CurrentWorkingDirectory,
        git_repo_root: find_git_root(&inputs.cwd),
    }
}

pub fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn git_ignore_warning(trace_root: &Path) -> Option<GitIgnoreWarning> {
    let git_repo_root = find_git_root(trace_root)?;
    Some(GitIgnoreWarning {
        trace_root: trace_root.to_path_buf(),
        ignored: is_trace_root_ignored(&git_repo_root, trace_root),
        git_repo_root,
    })
}

fn is_trace_root_ignored(git_repo_root: &Path, trace_root: &Path) -> bool {
    let Ok(relative) = trace_root.strip_prefix(git_repo_root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    let Ok(gitignore) = fs::read_to_string(git_repo_root.join(".gitignore")) else {
        return false;
    };
    gitignore.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && (line == relative
                || line == format!("{relative}/")
                || line == TRACE_DIR_NAME
                || line == format!("{TRACE_DIR_NAME}/"))
    })
}
