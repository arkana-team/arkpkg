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
        let _ = self.init();
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file_path)
        {
            let _ = writeln!(file, "[{}] {}", timestamp, escaped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
