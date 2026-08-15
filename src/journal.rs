use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ensure_not_symlink, ensure_private_dir, ensure_private_file};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "journal.jsonl";
const JOURNAL_LOCK_FILE: &str = "journal.lock";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JournalEvent {
    pub schema_version: u32,
    pub id: String,
    pub timestamp_ms: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub payload: Value,
}

impl JournalEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Result<Self, String> {
        let kind = kind.into();
        if kind.trim().is_empty() {
            return Err("journal event kind must not be empty".to_owned());
        }
        Ok(Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            id: new_event_id()?,
            timestamp_ms: unix_timestamp_ms()?,
            kind,
            turn_id: None,
            parent_id: None,
            run_id: None,
            surface: None,
            cwd: None,
            payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Journal {
    root: PathBuf,
}

impl Journal {
    pub fn for_home(home: &Path) -> Self {
        Self::at_root(state_root_for_home(home))
    }

    pub fn at_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(JOURNAL_FILE)
    }

    pub fn append(&self, event: &JournalEvent) -> Result<(), String> {
        validate_event(event)?;
        self.prepare_root()?;
        let _lock = JournalAppendLock::acquire(&self.root)?;
        let path = self.path();
        ensure_not_symlink(&path).map_err(|_| "journal path is unsafe".to_owned())?;

        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
            .map_err(|_| "unable to open journal".to_owned())?;
        ensure_private_regular_file(&file)?;
        let encoded = serde_json::to_vec(event).map_err(|_| "unable to encode journal event".to_owned())?;
        file.write_all(&encoded)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_data())
            .map_err(|_| "unable to append journal event".to_owned())?;
        ensure_private_file(&path).map_err(|_| "unable to protect journal".to_owned())?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<JournalEvent>, String> {
        let path = self.path();
        ensure_not_symlink(&path).map_err(|_| "journal path is unsafe".to_owned())?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("unable to read journal".to_owned()),
        };
        decode_committed_records(&bytes)
    }

    /// Remove only an incomplete final JSONL record. Invalid records before the
    /// final newline are committed corruption and are never rewritten by this repair.
    pub fn recover_incomplete_tail(&self) -> Result<bool, String> {
        self.prepare_root()?;
        let _lock = JournalAppendLock::acquire(&self.root)?;
        let path = self.path();
        ensure_not_symlink(&path).map_err(|_| "journal path is unsafe".to_owned())?;
        let mut bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("unable to read journal".to_owned()),
        };
        if bytes.is_empty() || bytes.ends_with(b"\n") {
            decode_committed_records(&bytes)?;
            return Ok(false);
        }

        let committed_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        decode_committed_records(&bytes[..committed_len])?;
        bytes.truncate(committed_len);

        let mut options = OpenOptions::new();
        options.write(true).truncate(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
            .map_err(|_| "unable to open journal for recovery".to_owned())?;
        ensure_private_regular_file(&file)?;
        file.write_all(&bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_data())
            .map_err(|_| "unable to recover journal tail".to_owned())?;
        ensure_private_file(&path).map_err(|_| "unable to protect journal".to_owned())?;
        Ok(true)
    }

    fn prepare_root(&self) -> Result<(), String> {
        ensure_not_symlink(&self.root).map_err(|_| "journal root is unsafe".to_owned())?;
        ensure_private_dir(&self.root).map_err(|_| "unable to secure journal root".to_owned())
    }
}

pub fn state_root_for_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("state"))
        .join("lucy")
}

fn validate_event(event: &JournalEvent) -> Result<(), String> {
    if event.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err("unsupported journal event schema".to_owned());
    }
    if event.id.trim().is_empty() || event.kind.trim().is_empty() {
        return Err("invalid journal event".to_owned());
    }
    Ok(())
}

fn decode_committed_records(bytes: &[u8]) -> Result<Vec<JournalEvent>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.ends_with(b"\n") {
        return Err("journal has an incomplete final record".to_owned());
    }

    let mut events = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let event = serde_json::from_slice::<JournalEvent>(line)
            .map_err(|_| "journal contains an invalid committed record".to_owned())?;
        validate_event(&event)?;
        events.push(event);
    }
    Ok(events)
}

fn new_event_id() -> Result<String, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "unable to generate journal event id".to_owned())?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(4 + random.len() * 2);
    id.push_str("evt-");
    for byte in random {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(id)
}

fn unix_timestamp_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before Unix epoch".to_owned())?
        .as_millis();
    u64::try_from(millis).map_err(|_| "system clock is outside supported range".to_owned())
}

fn ensure_private_regular_file(file: &File) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|_| "unable to inspect journal file".to_owned())?;
    if !metadata.is_file() {
        return Err("journal path is not a regular file".to_owned());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err("journal file is not private".to_owned());
    }
    Ok(())
}

struct JournalAppendLock {
    #[cfg(unix)]
    file: File,
}

impl JournalAppendLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(JOURNAL_LOCK_FILE);
        ensure_not_symlink(&path).map_err(|_| "journal append lock is unsafe".to_owned())?;

        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
            let file = options
                .open(&path)
                .map_err(|_| "unable to open journal append lock".to_owned())?;
            ensure_private_regular_file(&file)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err("unable to acquire journal append lock".to_owned());
            }
            ensure_private_file(&path)
                .map_err(|_| "unable to protect journal append lock".to_owned())?;
            return Ok(Self { file });
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

impl Drop for JournalAppendLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn temporary_root(name: &str) -> PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).expect("random root");
        let suffix = random.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        std::env::temp_dir().join(format!("lucy-journal-{name}-{suffix}"))
    }

    fn event(text: &str) -> JournalEvent {
        JournalEvent::new("user_message", serde_json::json!({"text": text})).expect("event")
    }

    #[test]
    fn append_and_read_preserve_exact_event_order() {
        let root = temporary_root("order");
        let journal = Journal::at_root(root.clone());
        let first = event("first");
        let second = event("second");

        journal.append(&first).expect("append first");
        journal.append(&second).expect("append second");

        assert_eq!(journal.read_all().expect("read"), vec![first, second]);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn journal_schema_contains_facts_without_semantic_memory_fields() {
        let event = JournalEvent::new("tool_result", serde_json::json!({"status": 0}))
            .expect("event");
        let value = serde_json::to_value(event).expect("value");
        let object = value.as_object().expect("object");

        for forbidden in ["summary", "topic", "importance", "lesson", "persona", "memory"] {
            assert!(!object.contains_key(forbidden), "unexpected semantic field: {forbidden}");
        }
        assert!(object.contains_key("id"));
        assert!(object.contains_key("timestamp_ms"));
        assert!(object.contains_key("kind"));
        assert!(object.contains_key("payload"));
    }

    #[test]
    fn incomplete_tail_is_loud_and_recoverable_without_touching_committed_records() {
        let root = temporary_root("tail");
        let journal = Journal::at_root(root.clone());
        let committed = event("committed");
        journal.append(&committed).expect("append");
        let path = journal.path();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open tail")
            .write_all(br#"{"schema_version":1,"id":"partial""#)
            .expect("write tail");

        assert_eq!(
            journal.read_all().expect_err("tail must be loud"),
            "journal has an incomplete final record"
        );
        assert!(journal.recover_incomplete_tail().expect("recover"));
        assert_eq!(journal.read_all().expect("read"), vec![committed]);
        assert!(!journal.recover_incomplete_tail().expect("already clean"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recovery_refuses_invalid_committed_records() {
        let root = temporary_root("corrupt");
        fs::create_dir_all(&root).expect("root");
        let journal = Journal::at_root(root.clone());
        fs::write(journal.path(), b"not-json\npartial").expect("corrupt journal");

        assert_eq!(
            journal.recover_incomplete_tail().expect_err("must refuse"),
            "journal contains an invalid committed record"
        );
        assert_eq!(fs::read(journal.path()).expect("unchanged"), b"not-json\npartial");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_appenders_serialize_complete_records() {
        let root = temporary_root("concurrent");
        let workers = 4;
        let per_worker = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut threads = Vec::new();

        for worker in 0..workers {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let journal = Journal::at_root(root);
                barrier.wait();
                for index in 0..per_worker {
                    journal
                        .append(&event(&format!("{worker}:{index}")))
                        .expect("append");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("worker");
        }

        let journal = Journal::at_root(root.clone());
        let events = journal.read_all().expect("read");
        assert_eq!(events.len(), workers * per_worker);
        let ids = events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), events.len());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
