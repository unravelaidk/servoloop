//! Versioned snapshot envelope and consistency checks.
//!
//! Snapshot version 2 stores the journal watermark beside the core session.
//! Callers only receive a snapshot when its session identity, watermark, and
//! unresolved-outcome checks all pass. Replacement is performed by the
//! facade with a durable temporary file and atomic rename.

use crate::{filesystem, journal, Result, StoreError};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub(crate) version: u32,
    pub(crate) journal_sequence: u64,
    pub(crate) session: servoloop_core::Session,
}

pub(crate) fn write(
    dir: &Path,
    session: &servoloop_core::Session,
    journal_sequence: u64,
    unresolved: bool,
) -> Result<()> {
    if unresolved
        || session
            .messages
            .iter()
            .any(|message| matches!(message, servoloop_core::Message::ToolUnknown { .. }))
    {
        return Err(StoreError::Unresolved);
    }
    let data = serde_json::to_vec(&Envelope {
        version: 2,
        journal_sequence,
        session: session.clone(),
    })?;
    if data.len() as u64 > journal::MAX_JOURNAL {
        return Err(StoreError::TooLarge);
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&data)?;
    temp.as_file().sync_all()?;
    let target = dir.join("snapshot.json");
    filesystem::reject_symlink(&target)?;
    temp.persist(&target)
        .map_err(|error| StoreError::Io(error.into()))?;
    filesystem::sync_directory(dir)
}

pub(crate) fn read(path: &Path, id: &str) -> Result<Envelope> {
    filesystem::reject_symlink(path)?;
    let mut data = Vec::new();
    File::open(path)?
        .take(journal::MAX_JOURNAL + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > journal::MAX_JOURNAL {
        return Err(StoreError::TooLarge);
    }
    let snapshot: Envelope = serde_json::from_slice(&data)?;
    if snapshot.version != 2 {
        return Err(StoreError::UnsupportedVersion(snapshot.version));
    }
    if snapshot.session.id != id {
        return Err(StoreError::InvalidJournal(
            "snapshot session mismatch".into(),
        ));
    }
    Ok(snapshot)
}

pub(crate) fn validate(
    snapshot: Envelope,
    journal_sequence: u64,
    unresolved: bool,
) -> Result<servoloop_core::Session> {
    if unresolved
        || snapshot
            .session
            .messages
            .iter()
            .any(|message| matches!(message, servoloop_core::Message::ToolUnknown { .. }))
    {
        return Err(StoreError::Unresolved);
    }
    if snapshot.journal_sequence != journal_sequence {
        return Err(StoreError::InvalidJournal(
            "snapshot journal watermark does not match journal".into(),
        ));
    }
    Ok(snapshot.session)
}
