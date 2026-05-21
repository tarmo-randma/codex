use std::path::Path;
use std::path::PathBuf;

use crate::recorder::TraceOwner;
use crate::recorder::TraceRecorderInner;
use crate::schema::OwnerRecord;
use crate::writer;

#[derive(Debug, Clone)]
pub(super) struct OwnerScope {
    pub(super) id: String,
    pub(super) dir: PathBuf,
    pub(super) relative_dir: String,
    pub(super) metadata_file: String,
    pub(super) record: OwnerRecord,
}

pub(super) fn write_owner_record(_root: &Path, owner: &OwnerScope) {
    let _ = writer::write_json_pretty(&owner.dir.join(&owner.metadata_file), &owner.record);
}

pub(super) fn activate_owner_scope(inner: &mut TraceRecorderInner, owner: &TraceOwner) {
    let Some(current_owner) = owner_scope_from_handle(inner, owner) else {
        return;
    };
    if inner
        .current_owner
        .as_ref()
        .is_some_and(|active| active.relative_dir == current_owner.relative_dir)
    {
        inner.current_owner = Some(current_owner);
        return;
    }
    if let Some(previous_owner) = inner.current_owner.take() {
        inner.owner_stack.push(previous_owner);
    }
    inner.current_owner = Some(current_owner);
}

pub(super) fn update_owner_record(
    root: &Path,
    owner: &TraceOwner,
    update: impl FnOnce(&mut OwnerRecord),
) {
    let path = root.join(&owner.path).join(&owner.metadata_file);
    let Some(mut record) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<OwnerRecord>(&contents).ok())
    else {
        return;
    };
    update(&mut record);
    let _ = writer::write_json_pretty(&path, &record);
}

pub(super) fn update_owner_scope_record(
    inner: &mut TraceRecorderInner,
    owner: &TraceOwner,
    update: impl FnOnce(&mut OwnerRecord),
) {
    if let Some(active) = &mut inner.current_owner
        && active.relative_dir == owner.path
    {
        update(&mut active.record);
        write_owner_record(&inner.root, active);
        return;
    }
    if let Some(stacked) = inner
        .owner_stack
        .iter_mut()
        .find(|stacked| stacked.relative_dir == owner.path)
    {
        update(&mut stacked.record);
        write_owner_record(&inner.root, stacked);
        return;
    }
    update_owner_record(&inner.root, owner, update);
}

pub(super) fn owner_scope_from_handle(
    inner: &TraceRecorderInner,
    owner: &TraceOwner,
) -> Option<OwnerScope> {
    let path = inner.root.join(&owner.path).join(&owner.metadata_file);
    let record = std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<OwnerRecord>(&contents).ok())?;
    Some(OwnerScope {
        id: owner.id.clone(),
        dir: inner.root.join(&owner.path),
        relative_dir: owner.path.clone(),
        metadata_file: owner.metadata_file.clone(),
        record,
    })
}
