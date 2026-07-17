//! Bounded in-memory error log, optionally mirrored to a file.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use super::Event;

pub(super) const ERROR_LOG_CAP: usize = 300;

pub(super) struct ErrorLog {
    inner: Mutex<ErrorLogInner>,
    path: Option<PathBuf>,
}

struct ErrorLogInner {
    ring: VecDeque<Event>,
    file_lines: usize,
}

impl ErrorLog {
    pub(super) fn memory() -> Self {
        Self {
            inner: Mutex::new(ErrorLogInner { ring: VecDeque::new(), file_lines: 0 }),
            path: None,
        }
    }

    pub(super) fn load(path: PathBuf) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let file_lines = text.lines().count();
        let mut ring: VecDeque<Event> =
            text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        while ring.len() > ERROR_LOG_CAP {
            ring.pop_front();
        }
        Self { inner: Mutex::new(ErrorLogInner { ring, file_lines }), path: Some(path) }
    }

    pub(super) fn push(&self, event: &Event) {
        let mut g = self.inner.lock().expect("error log lock");
        g.ring.push_back(event.clone());
        while g.ring.len() > ERROR_LOG_CAP {
            g.ring.pop_front();
        }
        let Some(path) = &self.path else { return };
        let result = if g.file_lines >= ERROR_LOG_CAP * 2 {
            g.file_lines = g.ring.len();
            let body: String =
                g.ring.iter().filter_map(|e| serde_json::to_string(e).ok()).fold(
                    String::new(),
                    |mut acc, line| {
                        acc.push_str(&line);
                        acc.push('\n');
                        acc
                    },
                );
            std::fs::write(path, body)
        } else {
            g.file_lines += 1;
            serde_json::to_string(event).map_err(std::io::Error::other).and_then(|line| {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{line}")
            })
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %path.display(), "persisting error log");
        }
    }

    pub(super) fn snapshot(&self) -> Vec<Event> {
        self.inner.lock().expect("error log lock").ring.iter().cloned().collect()
    }

    pub(super) fn clear(&self) -> usize {
        let mut g = self.inner.lock().expect("error log lock");
        let n = g.ring.len();
        g.ring.clear();
        g.file_lines = 0;
        if let Some(path) = &self.path {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "clearing error log");
                }
            }
        }
        n
    }
}
