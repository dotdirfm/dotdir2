use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read},
    sync::{Arc, Mutex},
};

const MAX_LINES: usize = 200;
const MAX_LINE_BYTES: usize = 2048;

#[derive(Clone, Default)]
pub(crate) struct ExtensionLogs(Arc<Mutex<LogBuffer>>);

#[derive(Default)]
struct LogBuffer {
    lines: VecDeque<String>,
    changed: bool,
}

impl ExtensionLogs {
    pub fn append(&self, source: &str, message: &str) {
        let time = chrono::Local::now().format("%H:%M:%S");
        let mut buffer = self.0.lock().unwrap();
        for line in message.lines() {
            if buffer.lines.len() == MAX_LINES {
                buffer.lines.pop_front();
            }
            let end = line.floor_char_boundary(MAX_LINE_BYTES);
            let suffix = if end < line.len() { "…" } else { "" };
            buffer
                .lines
                .push_back(format!("{time} [{source}] {}{suffix}", &line[..end]));
            buffer.changed = true;
        }
    }

    pub fn capture(&self, source: String, output: impl Read + Send + 'static) {
        let logs = self.clone();
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
                logs.append(&source, &String::from_utf8_lossy(&line));
                if size > MAX_LINE_BYTES
                    && !line.ends_with(b"\n")
                    && output.skip_until(b'\n').is_err()
                {
                    break;
                }
            }
        });
    }

    pub fn snapshot(&self) -> String {
        self.0.lock().unwrap().lines.make_contiguous().join("\n")
    }

    pub fn take_changed(&self) -> bool {
        std::mem::take(&mut self.0.lock().unwrap().changed)
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
}
