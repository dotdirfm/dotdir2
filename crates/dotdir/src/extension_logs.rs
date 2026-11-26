use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

const MAX_LINES: usize = 200;
const MAX_LINE_BYTES: usize = 2048;
pub(crate) const ERROR_LOG_NAME: &str = "dotdir.log";

#[derive(Clone, Default)]
pub(crate) struct ExtensionLogs(Arc<Mutex<LogBuffer>>);

#[derive(Default)]
struct LogBuffer {
    lines: VecDeque<(Arc<str>, String)>,
    groups: BTreeSet<String>,
    changed: bool,
    error_file: Option<File>,
}

#[derive(Clone)]
pub(crate) struct LogSource {
    logs: ExtensionLogs,
    group: Arc<str>,
    label: Arc<str>,
}

impl ExtensionLogs {
    pub fn with_error_file(path: &Path) -> Self {
        let logs = Self::default();
        let open = || {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)
        };
        match open() {
            Ok(file) => logs.0.lock().unwrap().error_file = Some(file),
            Err(error) => logs.append(
                "dotdir",
                &format!("Could not open {}: {error}", path.display()),
            ),
        }
        logs
    }

    pub fn register(&self, group: &str) {
        self.0.lock().unwrap().groups.insert(group.into());
    }

    pub fn source(&self, group: &str, label: &str) -> LogSource {
        LogSource {
            logs: self.clone(),
            group: group.into(),
            label: label.into(),
        }
    }

    pub fn append(&self, source: &str, message: &str) {
        self.source(source, source).append(message);
    }

    pub fn capture(&self, source: String, output: impl Read + Send + 'static) {
        self.source(&source, &source).capture(output);
    }

    pub fn snapshot(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn groups(&self) -> Vec<String> {
        self.0.lock().unwrap().groups.iter().cloned().collect()
    }

    pub fn group_output(&self, group: &str) -> Option<String> {
        let buffer = self.0.lock().unwrap();
        buffer.groups.contains(group).then(|| {
            buffer
                .lines
                .iter()
                .filter(|(id, _)| id.as_ref() == group)
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    pub fn take_changed(&self) -> bool {
        std::mem::take(&mut self.0.lock().unwrap().changed)
    }
}

impl LogSource {
    pub fn append(&self, message: &str) {
        self.record(message, false);
    }

    pub fn error(&self, message: &str) {
        self.record(message, true);
    }

    fn record(&self, message: &str, is_error: bool) {
        use std::fmt::Write as _;

        let now = chrono::Local::now();
        let time = now.format("%H:%M:%S");
        let date = now.format("%Y-%m-%d");
        let mut buffer = self.logs.0.lock().unwrap();
        if !buffer.groups.contains(self.group.as_ref()) {
            buffer.groups.insert(self.group.to_string());
        }
        let mut disk_entry = String::new();
        for line in message.lines() {
            if buffer.lines.len() == MAX_LINES {
                buffer.lines.pop_front();
            }
            let end = line.floor_char_boundary(MAX_LINE_BYTES);
            let suffix = if end < line.len() { "…" } else { "" };
            let line = &line[..end];
            buffer.lines.push_back((
                self.group.clone(),
                format!("{time} [{}] {line}{suffix}", self.label),
            ));
            buffer.changed = true;
            if is_error && buffer.error_file.is_some() {
                let _ = writeln!(
                    disk_entry,
                    "{date} {time} ERROR [{}] [{}] {line}{suffix}",
                    self.group, self.label
                );
            }
        }
        if !disk_entry.is_empty()
            && let Some(file) = &mut buffer.error_file
            && let Err(error) = file.write_all(disk_entry.as_bytes())
        {
            buffer.error_file = None;
            drop(buffer);
            self.logs.append(
                "dotdir",
                &format!("Could not write the DotDir error log: {error}"),
            );
        }
    }

    pub fn capture(&self, output: impl Read + Send + 'static) {
        let source = self.clone();
        std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            let mut line = Vec::new();
            loop {
                line.clear();
                let Ok(size) = output
                    .by_ref()
                    .take(MAX_LINE_BYTES as u64 + 1)
                    .read_until(b'\n', &mut line)
                else {
                    break;
                };
                if size == 0 {
                    break;
                }
                source.append(&String::from_utf8_lossy(&line));
                if size > MAX_LINE_BYTES
                    && !line.ends_with(b"\n")
                    && output.skip_until(b'\n').is_err()
                {
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtensionLogs, MAX_LINES};

    #[test]
    fn retains_the_latest_log_lines_in_order() {
        let logs = ExtensionLogs::default();
        logs.append("fixture", "Discarded startup message");
        for entry in 0..MAX_LINES {
            logs.append("fixture", &format!("Message {entry}"));
        }

        let snapshot = logs.snapshot();
        assert_eq!(snapshot.lines().count(), MAX_LINES);
        assert!(
            snapshot
                .lines()
                .next()
                .unwrap()
                .ends_with("[fixture] Message 0")
        );
        assert!(snapshot.ends_with(&format!("[fixture] Message {}", MAX_LINES - 1)));
    }

    #[test]
    fn error_log_retains_errors_across_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile/dotdir.log");
        let first = ExtensionLogs::with_error_file(&path);
        let source = first.source("test.extension", "VS Code commands");
        source.append("Starting fixture");
        source.error("Fixture failed\n  at example.js:10");
        assert!(first.snapshot().contains("Starting fixture"));
        drop((source, first));

        ExtensionLogs::with_error_file(&path)
            .source("dotdir", "DotDir")
            .error("Second session");
        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("ERROR [test.extension] [VS Code commands] Fixture failed"));
        assert!(log.contains("at example.js:10"));
        assert!(log.contains("ERROR [dotdir] [DotDir] Second session"));
        assert!(!log.contains("Starting fixture"));
    }
}
