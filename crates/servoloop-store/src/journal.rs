//! Journal wire records and state-machine validation.
//!
//! This module owns the append-only journal's bounded wire format and its
//! intent/result transitions. It deliberately has no tool or model dispatch
//! capability: an unknown result can only be resolved by the trusted caller
//! in the store facade.

use crate::{filesystem, Result, StoreError, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    io::{BufReader, Read},
    path::Path,
};

pub(crate) const MAX_RECORD: usize = 64 * 1024;
pub(crate) const MAX_RECORDS: usize = 100_000;
pub(crate) const MAX_JOURNAL: u64 = 64 * 1024 * 1024;

/// The stable newline-delimited journal record. Keep this wire shape string
/// based for compatibility with existing CLI callers and journal files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalRecord {
    pub version: u32,
    pub sequence: u64,
    pub session_id: String,
    pub intent_id: String,
    pub kind: String,
    pub arguments: serde_json::Value,
    /// `None` on a result is an explicit unknown outcome.
    pub outcome: Option<String>,
}

pub(crate) fn append(
    path: &Path,
    session_dir: &Path,
    mut record: JournalRecord,
    existing: &[JournalRecord],
    trusted_resolution: bool,
) -> Result<()> {
    if record.version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedVersion(record.version));
    }
    validate_intent_id(&record.intent_id)?;
    let lock_path = path.with_extension("lock");
    let _lock = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_) => filesystem::FileLock(lock_path),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(StoreError::Busy)
        }
        Err(error) => return Err(error.into()),
    };
    filesystem::reject_symlink(path)?;
    record.sequence = existing.len() as u64 + 1;
    let mut candidate = existing.to_vec();
    candidate.push(record.clone());
    validate(&record.session_id, &candidate, trusted_resolution)?;
    let bytes = serde_json::to_vec(&record)?;
    if bytes.len() > MAX_RECORD {
        return Err(StoreError::TooLarge);
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    filesystem::sync_directory(session_dir)
}

pub(crate) fn read(path: &Path, session: &str) -> Result<Vec<JournalRecord>> {
    filesystem::reject_symlink(path)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    if fs::metadata(path)?.len() > MAX_JOURNAL {
        return Err(StoreError::TooLarge);
    }
    let mut records = Vec::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let mut count = 0;
        for _ in 0..=MAX_RECORD {
            let mut byte = [0_u8; 1];
            let read = reader.read(&mut byte)?;
            if read == 0 {
                break;
            }
            raw.push(byte[0]);
            count += 1;
            if byte[0] == b'\n' {
                break;
            }
        }
        if count == 0 {
            break;
        }
        if raw.len() > MAX_RECORD + 1 || raw.last() != Some(&b'\n') {
            return Err(StoreError::InvalidJournal(
                "record is not newline terminated or is too large".into(),
            ));
        }
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        records.push(serde_json::from_slice(&raw)?);
        if records.len() > MAX_RECORDS {
            return Err(StoreError::TooLarge);
        }
    }
    validate(session, &records, true)?;
    Ok(records)
}

fn validate_intent_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || id == "."
        || id == ".."
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Err(StoreError::InvalidId)
    } else {
        Ok(())
    }
}

pub(crate) fn unresolved(records: &[JournalRecord]) -> Result<Vec<JournalRecord>> {
    let mut out = Vec::new();
    for record in records {
        if record.kind == "intent" {
            out.push(record.clone());
        }
    }
    for record in records
        .iter()
        .filter(|record| record.kind == "result" && record.outcome.is_some())
    {
        out.retain(|intent| intent.intent_id != record.intent_id);
    }
    Ok(out)
}

pub(crate) fn validate(
    session: &str,
    records: &[JournalRecord],
    trusted_resolution: bool,
) -> Result<()> {
    let mut history = BTreeSet::new();
    let mut states = BTreeMap::<String, u8>::new();
    for (expected, record) in (1_u64..).zip(records) {
        if record.version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion(record.version));
        }
        if record.session_id != session || record.sequence != expected {
            return Err(StoreError::InvalidJournal(
                "session or sequence mismatch".into(),
            ));
        }
        if !history.insert(record.intent_id.clone()) && record.kind == "intent" {
            return Err(StoreError::InvalidJournal(
                "duplicate intent id in history".into(),
            ));
        }
        match record.kind.as_str() {
            "intent" if record.outcome.is_none() && !states.contains_key(&record.intent_id) => {
                states.insert(record.intent_id.clone(), 1);
            }
            "result"
                if states.get(&record.intent_id) == Some(&1)
                    && (record.outcome.is_none()
                        || record.outcome.as_deref() == Some("verified")) =>
            {
                states.insert(
                    record.intent_id.clone(),
                    if record.outcome.is_some() { 2 } else { 3 },
                );
            }
            "result"
                if trusted_resolution
                    && states.get(&record.intent_id) == Some(&3)
                    && record.outcome.as_deref() == Some("verified") =>
            {
                states.insert(record.intent_id.clone(), 2);
            }
            _ => {
                return Err(StoreError::InvalidJournal(
                    "invalid state transition".into(),
                ))
            }
        }
    }
    Ok(())
}
