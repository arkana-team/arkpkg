use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::errors::Result;

/// Log manager handling persistent append-only logs in `/var/log/arkpkg.log`.
#[derive(Debug, Clone)]
pub struct Logger {
    log_file_path: PathBuf,
}

impl Logger {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            log_file_path: root.as_ref().join("var/log/arkpkg.log"),
        }
    }

    /// Ensures the log directory exists.
    pub fn init(&self) -> Result<()> {
        if let Some(parent) = self.log_file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
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
        let _ = self.init();
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file_path)
        {
            let _ = write!(file, "[{}] {}", timestamp, line);
        }
    }
}
