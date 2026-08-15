use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::config::{ensure_not_symlink, ensure_private_dir, ensure_private_file};
use crate::journal::state_root_for_home;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const ATTENTION_LOCK_FILE: &str = "attention.lock";

/// Serializes one normal Lucy turn across processes without creating a memory
/// namespace. The lease is held only while a turn owns the single global
/// attention stream; idle TUI/gateway processes do not hold it.
#[derive(Debug)]
pub(crate) struct AttentionLease {
    #[cfg(unix)]
    file: File,
}

impl AttentionLease {
    pub(crate) fn acquire(home: &Path) -> Result<Self, String> {
        let root = state_root_for_home(home);
        ensure_not_symlink(&root).map_err(|_| "attention state root is unsafe".to_owned())?;
        ensure_private_dir(&root)
            .map_err(|_| "unable to secure attention state root".to_owned())?;
        let path = root.join(ATTENTION_LOCK_FILE);
        ensure_not_symlink(&path).map_err(|_| "attention lock is unsafe".to_owned())?;

        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
            let file = options
                .open(&path)
                .map_err(|_| "unable to open attention lock".to_owned())?;
            ensure_private_file(&path)
                .map_err(|_| "unable to protect attention lock".to_owned())?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err("unable to acquire attention lock".to_owned());
            }
            return Ok(Self { file });
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

impl Drop for AttentionLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
