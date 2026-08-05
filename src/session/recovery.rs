use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{ensure_not_symlink, ensure_private_dir, ensure_private_file, lucy_dir};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(super) fn recover_journals(home: &Path, session_id: &str) -> Result<(), String> {
    let transcript = super::sessions_dir(home).join(format!("{session_id}.jsonl"));
    recover_tail(home, session_id, "session", &transcript, true)?;
    let lifecycle = lucy_dir(home)
        .join("turns")
        .join(format!("{session_id}.jsonl"));
    recover_tail(home, session_id, "turn", &lifecycle, false)
}

fn recover_tail(
    home: &Path,
    session_id: &str,
    journal: &str,
    path: &Path,
    required: bool,
) -> Result<(), String> {
    ensure_not_symlink(path).map_err(|_| format!("{journal} journal is unsafe"))?;
    if !path.exists() {
        return if required {
            Err("session not found".to_owned())
        } else {
            Ok(())
        };
    }
    ensure_private_file(path).map_err(|_| format!("{journal} journal is not private"))?;
    let raw = fs::read(path).map_err(|_| format!("unable to read {journal} journal"))?;
    if raw.is_empty() || raw.last() == Some(&b'\n') {
        return Ok(());
    }
    let Some(last_newline) = raw.iter().rposition(|byte| *byte == b'\n') else {
        return Err(format!("{journal} journal has no complete record"));
    };
    let committed_len = last_newline + 1;
    let tail = &raw[committed_len..];
    write_recovery_evidence(home, session_id, journal, tail)?;

    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .map_err(|_| format!("unable to recover {journal} journal"))?;
    file.set_len(committed_len as u64)
        .map_err(|_| format!("unable to recover {journal} journal"))?;
    file.sync_data()
        .map_err(|_| format!("unable to checkpoint recovered {journal} journal"))?;
    Ok(())
}

fn write_recovery_evidence(
    home: &Path,
    session_id: &str,
    journal: &str,
    tail: &[u8],
) -> Result<(), String> {
    let directory = lucy_dir(home).join("recovery");
    ensure_not_symlink(&directory)
        .map_err(|_| "session recovery directory is unsafe".to_owned())?;
    ensure_private_dir(&directory)
        .map_err(|_| "unable to secure session recovery directory".to_owned())?;
    let mut hasher = DefaultHasher::new();
    tail.hash(&mut hasher);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let path = directory.join(format!(
        "{session_id}-{journal}-{timestamp}-{:016x}.json",
        hasher.finish()
    ));
    let record = serde_json::json!({
        "record": "recovered_trailing_fragment",
        "version": 1,
        "session_id": session_id,
        "journal": journal,
        "bytes": tail.len(),
        "hash": format!("{:016x}", hasher.finish()),
    });
    let mut encoded = serde_json::to_vec(&record)
        .map_err(|_| "unable to encode session recovery evidence".to_owned())?;
    encoded.push(b'\n');
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| "unable to create session recovery evidence".to_owned())?;
    file.write_all(&encoded)
        .map_err(|_| "unable to write session recovery evidence".to_owned())?;
    file.sync_data()
        .map_err(|_| "unable to checkpoint session recovery evidence".to_owned())?;
    Ok(())
}
