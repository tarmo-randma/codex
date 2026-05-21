use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> std::io::Result<Self> {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = temp_base().join(format!(
            "codex-local-trace-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn temp_base() -> PathBuf {
    [PathBuf::from("/dev/shm"), std::env::temp_dir()]
        .into_iter()
        .find(|path| path.is_dir() && !has_git_ancestor(path))
        .unwrap_or_else(std::env::temp_dir)
}

fn has_git_ancestor(path: &Path) -> bool {
    let mut current = path.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
