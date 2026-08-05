use std::fs::{File, OpenOptions};
use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;

use crate::config::{ensure_not_symlink, ensure_private_dir, lucy_dir};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Debug)]
pub(super) struct SessionLease {
    file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl SessionLease {
    pub(super) fn acquire(home: &Path, session_id: &str) -> Result<Self, String> {
        let lucy = lucy_dir(home);
        ensure_not_symlink(&lucy)
            .map_err(|_| "session writer lease directory is unsafe".to_owned())?;
        ensure_private_dir(&lucy)
            .map_err(|_| "unable to secure session writer lease directory".to_owned())?;
        let directory = lucy.join("locks");
        ensure_not_symlink(&directory)
            .map_err(|_| "session writer lease directory is unsafe".to_owned())?;
        ensure_private_dir(&directory)
            .map_err(|_| "unable to secure session writer lease directory".to_owned())?;
        let path = directory.join(format!("{session_id}.lock"));
        ensure_not_symlink(&path).map_err(|_| "session writer lease is unsafe".to_owned())?;

        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            let file = options
                .open(&path)
                .map_err(|_| "unable to open session writer lease".to_owned())?;
            let metadata = file
                .metadata()
                .map_err(|_| "unable to inspect session writer lease".to_owned())?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
                return Err("session writer lease is not a private regular file".to_owned());
            }
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                    || error.raw_os_error() == Some(libc::EAGAIN)
                {
                    return Err("session is already open for writing".to_owned());
                }
                return Err("unable to acquire session writer lease".to_owned());
            }
            file.sync_data()
                .map_err(|_| "unable to checkpoint session writer lease".to_owned())?;
            Ok(Self { file })
        }

        #[cfg(not(unix))]
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        "session is already open for writing".to_owned()
                    } else {
                        "unable to acquire session writer lease".to_owned()
                    }
                })?;
            Ok(Self { file, path })
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn home() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lucy-session-lease-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("home");
        path
    }

    #[test]
    fn a_second_writer_fails_without_waiting() {
        let home = home();
        let first = SessionLease::acquire(&home, "session").expect("first lease");
        let error = SessionLease::acquire(&home, "session").expect_err("second writer must fail");
        assert_eq!(error, "session is already open for writing");
        drop(first);
        SessionLease::acquire(&home, "session").expect("lease after release");
        std::fs::remove_dir_all(home).expect("cleanup");
    }
}
