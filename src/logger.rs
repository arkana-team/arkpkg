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
        self.append_log(&format!("INFO {}", msg));
    }

    /// Logs a specific action (e.g., `COPY /usr/bin/bash`).
    pub fn action(&self, action: &str, target: &str) {
        self.append_log(&format!("{} {}", action, target));
    }

    /// Prints and persists one record, escaping untrusted controls before adding a newline.
    fn append_log(&self, line: &str) {
        let mut escaped = String::with_capacity(line.len());
        for ch in line.chars() {
            if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                escaped.extend(ch.escape_default());
            } else {
                escaped.push(ch);
            }
        }
        println!("{}", escaped);
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Ok(mut file) = self.open_log_file() {
            let record = format!("[{}] {}\n", timestamp, escaped);
            let _ = file.write_all(record.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn log_calls_produce_single_records_with_escaped_controls() {
        let root = TempDir::new().unwrap();
        let logger = Logger::new(root.path());
        logger.info("Reading package\n[forged] INFO DONE\r\t\x1b\u{85}\u{2028}\u{2029}");
        logger.action("COPY\nINFO DONE", "/usr/bin/tool\r\nREMOVE /etc/passwd");
        logger.action("COPY", "/usr/bin/café 'tool' \\name");

        let log = fs::read_to_string(root.path().join("var/log/arkpkg.log")).unwrap();
        assert!(log.ends_with('\n'));
        assert!(!log.chars().any(|c| c.is_control() && c != '\n'));
        let messages: Vec<_> = log
            .lines()
            .map(|line| {
                let (timestamp, message) = line.split_once("] ").unwrap();
                chrono::DateTime::parse_from_rfc3339(timestamp.strip_prefix('[').unwrap()).unwrap();
                message
            })
            .collect();
        assert_eq!(
            messages,
            [
                "INFO Reading package\\n[forged] INFO DONE\\r\\t\\u{1b}\\u{85}\\u{2028}\\u{2029}",
                "COPY\\nINFO DONE /usr/bin/tool\\r\\nREMOVE /etc/passwd",
                "COPY /usr/bin/café 'tool' \\name",
            ]
        );
    }
}
