use codex_local_trace::blob::BlobStore;
use codex_local_trace::writer;
use serde_json::json;

mod support;

#[test]
fn writer_writes_pretty_json_and_compact_jsonl() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let json_path = temp.path().join("nested").join("value.json");
    writer::write_json_pretty(&json_path, &json!({"a": 1, "b": [true]}))?;
    let contents = std::fs::read_to_string(&json_path)?;
    assert!(contents.contains('\n'));
    assert!(contents.contains("  \"a\": 1"));

    let jsonl_path = temp.path().join("events.jsonl");
    writer::append_jsonl(&jsonl_path, &json!({"event": 1}))?;
    writer::append_jsonl(&jsonl_path, &json!({"event": 2}))?;
    assert_eq!(
        std::fs::read_to_string(jsonl_path)?,
        "{\"event\":1}\n{\"event\":2}\n"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn writer_uses_owner_only_permissions() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let temp = support::TempDir::new()?;
    let path = temp
        .path()
        .join("nested")
        .join("intermediate")
        .join("value.json");
    writer::write_json_pretty(&path, &json!({"a": 1}))?;

    assert_eq!(
        std::fs::metadata(path.parent().unwrap())?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(temp.path().join("nested"))?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[test]
fn blob_refs_are_lossless_and_deduplicate_content() -> Result<(), Box<dyn std::error::Error>> {
    let temp = support::TempDir::new()?;
    let store = BlobStore::new(temp.path());

    let first = store.put_text("hello", None)?;
    let second = store.put_text("hello", None)?;

    assert_eq!(first, second);
    assert_eq!(first.kind, "blob_ref");
    assert_eq!(first.bytes, 5);
    assert_eq!(
        std::fs::read_to_string(temp.path().join(&first.path))?,
        "hello"
    );
    Ok(())
}
