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
        self.log_line(&format!("INFO {}", msg));
    }

    /// Logs a specific action (e.g., `COPY /usr/bin/bash`).
    pub fn action(&self, action: &str, target: &str) {
        self.log_line(&format!("{} {}", action, target));
    }

    /// Escapes untrusted text before printing and appending one log record.
    fn log_line(&self, line: &str) {
        let mut escaped = String::with_capacity(line.len());
        for character in line.chars() {
            // Keep record separators and terminal controls out of logged fields.
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                escaped.extend(character.escape_default());
            } else {
                escaped.push(character);
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

    fn records(root: &Path) -> Vec<String> {
        let log = fs::read_to_string(root.join("var/log/arkpkg.log")).unwrap();
        assert!(log.ends_with('\n'));
        log.lines()
            .map(|line| {
                let (timestamp, record) = line.split_once("] ").unwrap();
                chrono::DateTime::parse_from_rfc3339(timestamp.strip_prefix('[').unwrap()).unwrap();
                record.to_string()
            })
            .collect()
    }

    #[test]
    fn escapes_controls_in_messages_actions_and_targets() {
        let root = TempDir::new().unwrap();
        let logger = Logger::new(root.path());
        logger.info("Reading package evil\r\nINFO DONE\t\0\u{1b}[2J");
        logger.action("CO\nPY\r", "/usr/bin/evil\nINFO DONE\u{85}\u{2028}\u{2029}");
        assert_eq!(
            records(root.path()),
            [
                r"INFO Reading package evil\r\nINFO DONE\t\u{0}\u{1b}[2J",
                r"CO\nPY\r /usr/bin/evil\nINFO DONE\u{85}\u{2028}\u{2029}",
            ]
        );
    }

    #[test]
    fn escapes_all_c0_and_c1_controls() {
        let root = TempDir::new().unwrap();
        let logger = Logger::new(root.path());
        let controls: String = ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}').collect();
        logger.info(&controls);
        logger.action(&controls, &controls);
        let records = records(root.path());
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .all(|record| !record.chars().any(char::is_control))
        );
        assert!(records[0].starts_with(r"INFO \u{0}\u{1}"));
        assert!(records[0].ends_with(r"\u{9f}"));
    }

    #[test]
    fn preserves_printable_text_and_record_format() {
        let root = TempDir::new().unwrap();
        let logger = Logger::new(root.path());
        logger.info(r#"Café 日本語 🦀 "quoted" C:\bin; it's useful!"#);
        logger.action("REMOVE DIR", "/usr/share/日本語");
        assert_eq!(
            records(root.path()),
            [
                r#"INFO Café 日本語 🦀 "quoted" C:\bin; it's useful!"#,
                "REMOVE DIR /usr/share/日本語",
            ]
        );
    }
}
