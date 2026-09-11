//! Small, deliberately conservative durable store for operator sessions.
//! Records are append-only and intent is synced before a caller dispatches.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: u32 = 1;
const MAX_RECORD: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid identifier")]
    InvalidId,
    #[error("unsupported schema version {0}")]
    UnsupportedVersion(u32),
    #[error("store I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("store data: {0}")]
    Data(#[from] serde_json::Error),
    #[error("record exceeds {MAX_RECORD} bytes")]
    TooLarge,
    #[error("journal lock timed out")]
    LockTimeout,
    #[error("journal is busy")]
    Busy,
    #[error("invalid journal: {0}")]
    InvalidJournal(String),
}
pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRecord {
    pub version: u32,
    pub sequence: u64,
    pub session_id: String,
    pub intent_id: String,
    pub kind: String,
    pub arguments: serde_json::Value,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if root.exists() && fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        fs::create_dir_all(&root)?;
        // Refuse a store whose root itself is a symlink.
        let meta = fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn safe_id(id: &str) -> Result<()> {
        if id.is_empty()
            || id.len() > 128
            || id == "."
            || id == ".."
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StoreError::InvalidId);
        }
        Ok(())
    }
    fn session_dir(&self, id: &str) -> Result<PathBuf> {
        Self::safe_id(id)?;
        Ok(self.root.join(id))
    }
    pub fn create_session(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id)?;
        fs::create_dir_all(&dir)?;
        let meta = fs::symlink_metadata(&dir)?;
        if meta.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        sync_directory(&dir)?;
        Ok(())
    }
    fn journal_path(&self, id: &str) -> Result<PathBuf> {
        self.create_session(id)?;
        Ok(self.session_dir(id)?.join("journal.ndjson"))
    }
    /// Append and sync one record. Call this before dispatching a tool.
    pub fn append(&self, mut record: JournalRecord) -> Result<()> {
        if record.version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion(record.version));
        }
        let path = self.journal_path(&record.session_id)?;
        let lock_path = path.with_extension("lock");
        let _lock = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => Lock(lock_path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StoreError::Busy)
            }
            Err(e) => return Err(e.into()),
        };
        if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptionsExt::mode(&mut options, 0o600);
        }
        let mut file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let existing = self.records(&record.session_id)?;
        record.sequence = existing.len() as u64 + 1;
        let mut candidate = existing;
        candidate.push(record.clone());
        validate_records(&record.session_id, &candidate)?;
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > MAX_RECORD {
            return Err(StoreError::TooLarge);
        }
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        sync_directory(&self.session_dir(&record.session_id)?)?;
        Ok(())
    }
    pub fn records(&self, id: &str) -> Result<Vec<JournalRecord>> {
        let path = self.journal_path(id)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        let file = File::open(path)?;
        let mut out = Vec::new();
        let mut reader = BufReader::new(file);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            let count = reader.read_until(b'\n', &mut raw)?;
            if count == 0 {
                break;
            }
            if raw.len() > MAX_RECORD + 1 {
                return Err(StoreError::TooLarge);
            }
            if raw.last() != Some(&b'\n') {
                return Err(StoreError::InvalidJournal(
                    "record is not newline terminated".into(),
                ));
            }
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            let r: JournalRecord = serde_json::from_slice(&raw)?;
            out.push(r);
        }
        validate_records(id, &out)?;
        Ok(out)
    }
    pub fn sessions(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let e = entry?;
            if e.file_type()?.is_dir() && !e.file_type()?.is_symlink() {
                if let Some(s) = e.file_name().to_str() {
                    if Self::safe_id(s).is_ok() {
                        out.push(s.to_string());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id)?;
        if dir.exists() {
            let m = fs::symlink_metadata(&dir)?;
            if m.file_type().is_symlink() {
                return Err(StoreError::InvalidId);
            }
            let lock = dir.join("journal.lock");
            if lock.exists() {
                return Err(StoreError::Busy);
            }
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
    pub fn unresolved(&self, id: &str) -> Result<Vec<JournalRecord>> {
        let records = self.records(id)?;
        let mut pending = Vec::new();
        for record in records {
            if record.kind == "intent" {
                pending.push(record);
            } else if record.outcome.as_deref() == Some("verified") {
                pending.retain(|r| r.intent_id != record.intent_id);
            }
        }
        Ok(pending)
    }
}

pub fn new_id(prefix: &str) -> String {
    format!(
        "{}-{}",
        prefix,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn validate_records(session: &str, records: &[JournalRecord]) -> Result<()> {
    let mut pending = std::collections::BTreeSet::new();
    for (expected, record) in (1_u64..).zip(records.iter()) {
        if record.version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion(record.version));
        }
        if record.session_id != session {
            return Err(StoreError::InvalidJournal("session mismatch".into()));
        }
        if record.sequence != expected {
            return Err(StoreError::InvalidJournal("nonmonotonic sequence".into()));
        }
        match record.kind.as_str() {
            "intent" => {
                if record.outcome.is_some() || !pending.insert(record.intent_id.clone()) {
                    return Err(StoreError::InvalidJournal(
                        "invalid or duplicate intent".into(),
                    ));
                }
            }
            "result" => {
                if !pending.remove(&record.intent_id)
                    || record.outcome.as_deref().is_some_and(|o| o != "verified")
                {
                    return Err(StoreError::InvalidJournal(
                        "orphan or invalid result".into(),
                    ));
                }
            }
            _ => return Err(StoreError::InvalidJournal("unknown record kind".into())),
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TempStore {
        dir: PathBuf,
        store: Store,
    }
    impl TempStore {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(new_id("store-test"));
            Self {
                store: Store::open(&dir).unwrap(),
                dir,
            }
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    fn record(session: &str) -> JournalRecord {
        JournalRecord {
            version: SCHEMA_VERSION,
            sequence: 0,
            session_id: session.into(),
            intent_id: "i".into(),
            kind: "intent".into(),
            arguments: serde_json::json!({"safe":true}),
            outcome: None,
        }
    }
    #[test]
    fn intent_is_readable_and_unresolved() {
        let dir = std::env::temp_dir().join(new_id("store-test"));
        let store = Store::open(&dir).unwrap();
        store.append(record("s")).unwrap();
        assert_eq!(store.unresolved("s").unwrap().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn ids_cannot_escape() {
        let dir = std::env::temp_dir().join(new_id("store-test"));
        let store = Store::open(&dir).unwrap();
        assert!(store.create_session("../outside").is_err());
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn truncated_journal_is_rejected() {
        let dir = std::env::temp_dir().join(new_id("store-test"));
        let store = Store::open(&dir).unwrap();
        store.create_session("s").unwrap();
        fs::write(dir.join("s/journal.ndjson"), b"{\"version\":1").unwrap();
        assert!(store.records("s").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn verified_result_resolves_intent() {
        let t = TempStore::new();
        t.store.append(record("s")).unwrap();
        t.store
            .append(JournalRecord {
                kind: "result".into(),
                outcome: Some("verified".into()),
                ..record("s")
            })
            .unwrap();
        assert!(t.store.unresolved("s").unwrap().is_empty());
    }
    #[test]
    fn unknown_result_remains_pending() {
        let t = TempStore::new();
        t.store.append(record("s")).unwrap();
        t.store
            .append(JournalRecord {
                kind: "result".into(),
                outcome: None,
                ..record("s")
            })
            .unwrap();
        assert_eq!(t.store.unresolved("s").unwrap().len(), 1);
    }
    #[test]
    fn orphan_result_is_rejected() {
        let t = TempStore::new();
        assert!(t
            .store
            .append(JournalRecord {
                kind: "result".into(),
                outcome: Some("verified".into()),
                ..record("s")
            })
            .is_err());
        assert!(t.store.records("s").unwrap().is_empty());
    }
    #[test]
    fn wrong_sequence_and_session_are_rejected() {
        let t = TempStore::new();
        t.store.create_session("s").unwrap();
        let path = t.dir.join("s/journal.ndjson");
        let mut r = record("other");
        r.sequence = 2;
        fs::write(path, format!("{}\n", serde_json::to_string(&r).unwrap())).unwrap();
        assert!(matches!(
            t.store.records("s"),
            Err(StoreError::InvalidJournal(_))
        ));
    }
    #[test]
    fn valid_json_without_newline_is_torn() {
        let t = TempStore::new();
        t.store.create_session("s").unwrap();
        fs::write(
            t.dir.join("s/journal.ndjson"),
            serde_json::to_vec(&record("s")).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            t.store.records("s"),
            Err(StoreError::InvalidJournal(_))
        ));
    }
    #[test]
    fn append_refuses_corrupt_existing_journal() {
        let t = TempStore::new();
        t.store.create_session("s").unwrap();
        fs::write(t.dir.join("s/journal.ndjson"), b"{\"version\":1}\n").unwrap();
        assert!(t.store.append(record("s")).is_err());
        assert_eq!(
            fs::read_to_string(t.dir.join("s/journal.ndjson"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
    #[cfg(unix)]
    #[test]
    fn journal_is_private_and_symlink_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempStore::new();
        t.store.append(record("s")).unwrap();
        let mode = fs::metadata(t.dir.join("s/journal.ndjson"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let target = t.dir.join("target");
        fs::write(&target, b"").unwrap();
        let journal = t.dir.join("s/journal.ndjson");
        fs::remove_file(&journal).unwrap();
        std::os::unix::fs::symlink(&target, &journal).unwrap();
        assert!(t.store.append(record("s")).is_err());
    }
}
