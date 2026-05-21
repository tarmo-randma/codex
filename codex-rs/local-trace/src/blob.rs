use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::writer;

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    pub bytes: usize,
    pub mime: String,
    pub sha256: String,
    pub source_path: Option<PathBuf>,
}

impl BlobStore {
    pub fn new(session_root: impl AsRef<Path>) -> Self {
        Self {
            root: session_root.as_ref().join("blobs"),
        }
    }

    pub fn put_text(&self, text: &str, source_path: Option<PathBuf>) -> Result<BlobRef> {
        self.put_bytes(text.as_bytes(), "text/plain", "txt", source_path)
    }

    pub fn put_json(
        &self,
        value: &impl Serialize,
        source_path: Option<PathBuf>,
    ) -> Result<BlobRef> {
        let bytes = serde_json::to_vec(value)?;
        self.put_bytes(&bytes, "application/json", "json", source_path)
    }

    pub fn put_bytes(
        &self,
        bytes: &[u8],
        mime: &str,
        extension: &str,
        source_path: Option<PathBuf>,
    ) -> Result<BlobRef> {
        let sha256 = sha256_hex(bytes);
        let file_name = format!("sha256-{sha256}.{extension}");
        let path = self.root.join(&file_name);
        if !path.exists() {
            writer::write_bytes(&path, bytes)?;
        }
        Ok(BlobRef {
            kind: "blob_ref".to_string(),
            path: format!("blobs/{file_name}"),
            bytes: bytes.len(),
            mime: mime.to_string(),
            sha256,
            source_path,
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
