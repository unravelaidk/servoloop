//! Conservative, fail-closed persistence for operator sessions.
//!
//! A journal intent is durable before its operation is dispatched.  An
//! unknown outcome is never treated as success.  This crate does not replay
//! tools and does not know how to redact secrets: callers must pass a redacted
//! [`unravel_agent_runtime::Session`] (or redact it with their own policy) to
//! [`Store::save_snapshot`]. Paths are checked before use and symlinked store
//! entries are rejected, but callers must place the root in a trusted parent:
//! no portable filesystem API can eliminate every parent-directory TOCTOU
//! race. Unix directory syncing is supported. Snapshot replacement uses the
//! `tempfile` crate's platform-specific atomic rename operation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

mod filesystem;
mod journal;
mod snapshot;

pub use journal::JournalRecord;

pub const SCHEMA_VERSION: u32 = 1;

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
    #[error("record exceeds 65536 bytes")]
    TooLarge,
    #[error("journal lock timed out")]
    LockTimeout,
    #[error("store is busy")]
    Busy,
    #[error("invalid journal: {0}")]
    InvalidJournal(String),
    #[error("session has unresolved tool outcomes")]
    Unresolved,
}
pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A process-wide (file-backed) session execution lease. Hold this for the
/// complete tool execution, not merely while appending journal records.
pub struct SessionGuard {
    path: PathBuf,
    session_id: String,
    store: Store,
}
impl SessionGuard {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn append(&self, record: JournalRecord) -> Result<()> {
        if record.session_id != self.session_id {
            return Err(StoreError::InvalidJournal(
                "record session does not match session lease".into(),
            ));
        }
        self.store.append_locked(record, false)
    }

    /// Save a terminal session while retaining this execution lease. This is
    /// the non-locking counterpart to `Store::save_snapshot`.
    pub fn save_snapshot(&self, session: &unravel_agent_runtime::Session) -> Result<()> {
        if session.id != self.session_id {
            return Err(StoreError::InvalidJournal(
                "snapshot session does not match session lease".into(),
            ));
        }
        self.store.save_snapshot_locked(session)
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if root.exists() && fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        fs::create_dir_all(&root)?;
        let meta = fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        // Do not chmod an existing user directory. New files are private.
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
            Err(StoreError::InvalidId)
        } else {
            Ok(())
        }
    }
    fn session_dir(&self, id: &str) -> Result<PathBuf> {
        Self::safe_id(id)?;
        Ok(self.root.join(id))
    }
    pub fn create_session(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id)?;
        fs::create_dir_all(&dir)?;
        if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
            return Err(StoreError::InvalidId);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(&dir)?.permissions().mode() & 0o777 != 0o700 {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        filesystem::sync_directory(&dir)
    }
    fn journal_path(&self, id: &str) -> Result<PathBuf> {
        self.create_session(id)?;
        Ok(self.session_dir(id)?.join("journal.ndjson"))
    }

    /// Acquire the lease used for the whole execution. `Busy` is deliberate;
    /// callers must not run a second actuator session concurrently.
    pub fn acquire_session(&self, id: &str) -> Result<SessionGuard> {
        let dir = self.session_dir(id)?;
        self.create_session(id)?;
        let path = dir.join("session.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(SessionGuard {
                path,
                session_id: id.to_owned(),
                store: self.clone(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(StoreError::Busy),
            Err(e) => Err(e.into()),
        }
    }
    /// Append using a short journal lock. For a full execution use the guard's
    /// `append`; this method remains compatible with existing callers.
    pub fn append(&self, record: JournalRecord) -> Result<()> {
        let guard = self.acquire_session(&record.session_id)?;
        guard.append(record)
    }
    fn append_locked(&self, record: JournalRecord, trusted_resolution: bool) -> Result<()> {
        let path = self.journal_path(&record.session_id)?;
        let existing = self.records(&record.session_id)?;
        journal::append(
            &path,
            &self.session_dir(&record.session_id)?,
            record,
            &existing,
            trusted_resolution,
        )
    }
    pub fn records(&self, id: &str) -> Result<Vec<JournalRecord>> {
        let path = self.journal_path(id)?;
        journal::read(&path, id)
    }
    pub fn sessions(&self) -> Result<Vec<String>> {
        filesystem::list_dirs(&self.root)
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        let _guard = self.acquire_session(id)?;
        let dir = self.session_dir(id)?;
        if dir.exists() {
            filesystem::reject_symlink(&dir)?;
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
    pub fn unresolved(&self, id: &str) -> Result<Vec<JournalRecord>> {
        let records = self.records(id)?;
        journal::unresolved(&records)
    }
    /// Trusted operator-only resolution. Model/tool dispatch code must not be
    /// given this capability. It is the sole path from unknown to verified.
    pub fn resolve_unknown(&self, id: &str, intent_id: &str) -> Result<()> {
        let guard = self.acquire_session(id)?;
        let records = self.records(id)?;
        let intent = records
            .iter()
            .find(|r| r.intent_id == intent_id && r.kind == "intent");
        if intent.is_none()
            || !journal::unresolved(&records)?
                .iter()
                .any(|r| r.intent_id == intent_id)
        {
            return Err(StoreError::InvalidJournal("intent is not unknown".into()));
        }
        guard.store.append_locked(
            JournalRecord {
                version: SCHEMA_VERSION,
                sequence: 0,
                session_id: id.into(),
                intent_id: intent_id.into(),
                kind: "result".into(),
                arguments: serde_json::json!({}),
                outcome: Some("verified".into()),
            },
            true,
        )
    }
    pub fn save_snapshot(&self, session: &unravel_agent_runtime::Session) -> Result<()> {
        let _guard = self.acquire_session(&session.id)?;
        _guard.save_snapshot(session)
    }

    fn save_snapshot_locked(&self, session: &unravel_agent_runtime::Session) -> Result<()> {
        Self::safe_id(&session.id)?;
        let dir = self.session_dir(&session.id)?;
        let records = self.records(&session.id)?;
        snapshot::write(
            &dir,
            session,
            records.len() as u64,
            !journal::unresolved(&records)?.is_empty(),
        )
    }
    pub fn load_snapshot(&self, id: &str) -> Result<unravel_agent_runtime::Session> {
        Self::safe_id(id)?;
        let dir = self.root.join(id);
        if !dir.is_dir() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            )));
        }
        let path = dir.join("snapshot.json");
        self.load_snapshot_file(id, path)
    }

    /// Load a snapshot while holding the caller's execution lease. This checks
    /// the journal watermark before returning any session state.
    pub fn load_snapshot_guarded(
        &self,
        guard: &SessionGuard,
    ) -> Result<unravel_agent_runtime::Session> {
        if guard.store.root != self.root {
            return Err(StoreError::Busy);
        }
        let path = self.session_dir(guard.session_id())?.join("snapshot.json");
        self.load_snapshot_file(guard.session_id(), path)
    }

    fn load_snapshot_file(
        &self,
        id: &str,
        path: PathBuf,
    ) -> Result<unravel_agent_runtime::Session> {
        let snapshot = snapshot::read(&path, id)?;
        let records = self.records(id)?;
        snapshot::validate(
            snapshot,
            records.len() as u64,
            !journal::unresolved(&records)?.is_empty(),
        )
    }
    pub fn snapshot_sessions(&self) -> Result<Vec<String>> {
        self.sessions().map(|v| {
            v.into_iter()
                .filter(|id| self.root.join(id).join("snapshot.json").is_file())
                .collect()
        })
    }
}

pub fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{}-{}",
        prefix,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        counter
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unravel_agent_runtime::{Message, Session};

    fn store() -> (Store, PathBuf) {
        let path = std::env::temp_dir().join(new_id("store-test"));
        (Store::open(&path).unwrap(), path)
    }
    fn intent(id: &str, key: &str) -> JournalRecord {
        JournalRecord {
            version: SCHEMA_VERSION,
            sequence: 0,
            session_id: id.into(),
            intent_id: key.into(),
            kind: "intent".into(),
            arguments: serde_json::json!({"redacted": true}),
            outcome: None,
        }
    }
    fn result(id: &str, key: &str, outcome: Option<&str>) -> JournalRecord {
        JournalRecord {
            kind: "result".into(),
            outcome: outcome.map(str::to_owned),
            ..intent(id, key)
        }
    }
    #[test]
    fn duplicate_completed_intent_is_rejected() {
        let (s, p) = store();
        s.append(intent("a", "x")).unwrap();
        s.append(result("a", "x", Some("verified"))).unwrap();
        assert!(s.append(intent("a", "x")).is_err());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn unknown_requires_trusted_resolution() {
        let (s, p) = store();
        s.append(intent("a", "x")).unwrap();
        s.append(result("a", "x", None)).unwrap();
        assert_eq!(s.unresolved("a").unwrap().len(), 1);
        assert!(s.append(result("a", "x", Some("verified"))).is_err());
        s.resolve_unknown("a", "x").unwrap();
        assert!(s.unresolved("a").unwrap().is_empty());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn orphan_and_bad_outcome_rejected() {
        let (s, p) = store();
        assert!(s.append(result("a", "x", Some("ok"))).is_err());
        assert!(s.append(result("a", "x", Some("verified"))).is_err());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn sequence_and_session_corruption_rejected() {
        let (s, p) = store();
        s.create_session("a").unwrap();
        fs::write(
            p.join("a/journal.ndjson"),
            serde_json::to_vec(&JournalRecord {
                sequence: 2,
                ..intent("a", "x")
            })
            .unwrap()
            .into_iter()
            .chain(*b"\n")
            .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(matches!(s.records("a"), Err(StoreError::InvalidJournal(_))));
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn truncated_and_oversized_records_rejected() {
        let (s, p) = store();
        s.create_session("a").unwrap();
        fs::write(p.join("a/journal.ndjson"), b"{\"version\":1").unwrap();
        assert!(s.records("a").is_err());
        fs::write(
            p.join("a/journal.ndjson"),
            vec![b'x'; journal::MAX_RECORD + 2],
        )
        .unwrap();
        assert!(s.records("a").is_err());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn snapshot_round_trip_and_listing() {
        let (s, p) = store();
        let session = Session::new("a");
        s.save_snapshot(&session).unwrap();
        assert_eq!(s.snapshot_sessions().unwrap(), vec!["a"]);
        assert_eq!(s.load_snapshot("a").unwrap().id, "a");
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn snapshot_temp_is_ignored() {
        let (s, p) = store();
        s.create_session("a").unwrap();
        fs::write(p.join("a/snapshot.tmp.crash"), b"broken").unwrap();
        assert!(s.snapshot_sessions().unwrap().is_empty());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn snapshot_rejects_unknown_fields() {
        let (s, p) = store();
        s.create_session("a").unwrap();
        fs::write(
            p.join("a/snapshot.json"),
            br#"{"version":1,"session":{"id":"a","messages":[]},"extra":true}"#,
        )
        .unwrap();
        assert!(matches!(s.load_snapshot("a"), Err(StoreError::Data(_))));
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn snapshot_unknown_is_not_recoverable() {
        let (s, p) = store();
        let mut session = Session::new("a");
        session.messages.push(Message::ToolUnknown {
            call_id: "c".into(),
            name: "tool".into(),
            reason: "lost".into(),
        });
        assert!(matches!(
            s.save_snapshot(&session),
            Err(StoreError::Unresolved)
        ));
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn journal_unknown_blocks_snapshot_load() {
        let (s, p) = store();
        s.save_snapshot(&Session::new("a")).unwrap();
        s.append(intent("a", "x")).unwrap();
        s.append(result("a", "x", None)).unwrap();
        assert!(matches!(s.load_snapshot("a"), Err(StoreError::Unresolved)));
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn guard_spans_append_and_delete() {
        let (s, p) = store();
        let guard = s.acquire_session("a").unwrap();
        assert!(matches!(s.acquire_session("a"), Err(StoreError::Busy)));
        assert!(matches!(s.delete("a"), Err(StoreError::Busy)));
        guard.append(intent("a", "x")).unwrap();
        drop(guard);
        s.delete("a").unwrap();
        assert!(s.sessions().unwrap().is_empty());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn guard_cannot_append_for_another_session() {
        let (s, p) = store();
        let guard = s.acquire_session("a").unwrap();
        assert!(guard.append(intent("b", "x")).is_err());
        drop(guard);
        let _ = fs::remove_dir_all(p);
    }
    #[cfg(unix)]
    #[test]
    fn files_are_private_and_symlinks_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (s, p) = store();
        s.append(intent("a", "x")).unwrap();
        assert_eq!(
            fs::metadata(p.join("a/journal.ndjson"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        symlink(p.join("a/journal.ndjson"), p.join("link")).unwrap();
        assert!(s.records("link").is_err());
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn invalid_ids_cannot_escape() {
        let (s, p) = store();
        assert!(s.create_session("../outside").is_err());
        assert!(s.create_session("a/b").is_err());
        let _ = fs::remove_dir_all(p);
    }
}
