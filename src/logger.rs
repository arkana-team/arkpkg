use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
#[cfg(unix)]
use rustix::io::Errno;

use crate::errors::Result;

/// Log manager handling persistent append-only logs in `/var/log/arkpkg.log`.
#[derive(Debug, Clone)]
pub struct Logger {
    root: PathBuf,
}

impl Logger {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Ensures the log directory exists, rejecting symlinks beneath the root.
    pub fn init(&self) -> Result<()> {
        self.open_log_directory()?;
        Ok(())
    }

    #[cfg(unix)]
    fn open_log_directory(&self) -> io::Result<File> {
        // The caller selects the root. Resolve each untrusted component beneath
        // it through directory handles so a pathname swap cannot redirect us.
        std::fs::create_dir_all(&self.root)?;
        let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut directory = File::from(open(&self.root, directory_flags, Mode::empty())?);
        for component in ["var", "log"] {
            match mkdirat(&directory, component, Mode::from_raw_mode(0o777)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            directory = File::from(openat(
                &directory,
                component,
                directory_flags | OFlags::NOFOLLOW,
                Mode::empty(),
            )?);
        }
        Ok(directory)
    }

    #[cfg(unix)]
    fn open_log_file(&self) -> io::Result<File> {
        let directory = self.open_log_directory()?;
        let file = File::from(openat(
            &directory,
            "arkpkg.log",
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::APPEND
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )?);
        // NONBLOCK avoids hanging on a FIFO before we can reject it here.
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log destination must be a regular file",
            ));
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    fn open_log_directory(&self) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure logging requires Unix directory-relative operations",
        ))
    }

    #[cfg(not(unix))]
    fn open_log_file(&self) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure logging requires Unix directory-relative operations",
        ))
    }

    /// Appends a log line to persistent log file and prints to stdout.
    pub fn info(&self, msg: &str) {
        println!("INFO {}", msg);
        self.append_log(&format!("INFO {}\n", msg));
    }

    /// Logs a specific action (e.g., `COPY /usr/bin/bash`).
    pub fn action(&self, action: &str, target: &str) {
        println!("{} {}", action, target);
        self.append_log(&format!("{} {}\n", action, target));
    }

    /// Appends text to the persistent log file.
    fn append_log(&self, line: &str) {
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Ok(mut file) = self.open_log_file() {
            let _ = write!(file, "[{}] {}", timestamp, line);
        }
    }
}
