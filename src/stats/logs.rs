//! Rotating log file: one current file plus one old file.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub(super) struct RotatingLog {
    path: PathBuf,
    old_path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    started_ms: Option<u64>,
    rotate_ms: u64,
}

impl RotatingLog {
    pub(super) fn open(path: PathBuf, rotate_hours: u32) -> Self {
        let old_path = path.with_extension("old.jsonl");
        let started_ms = first_line_ts(&path);
        Self {
            path,
            old_path,
            inner: Mutex::new(Inner {
                file: None,
                started_ms,
                rotate_ms: hours_ms(rotate_hours),
            }),
        }
    }

    pub(super) fn set_rotate_hours(&self, hours: u32) {
        self.inner.lock().expect("log lock").rotate_ms = hours_ms(hours);
    }

    pub(super) fn append(&self, ts_ms: u64, line: &str) {
        let mut g = self.inner.lock().expect("log lock");
        if g.started_ms.is_some_and(|s| ts_ms.saturating_sub(s) >= g.rotate_ms) {
            g.file = None;
            if let Err(e) = std::fs::rename(&self.path, &self.old_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, path = %self.path.display(), "rotating log");
                }
            }
            g.started_ms = None;
        }
        if g.file.is_none() {
            match OpenOptions::new().create(true).append(true).open(&self.path) {
                Ok(f) => g.file = Some(f),
                Err(e) => {
                    tracing::warn!(error = %e, path = %self.path.display(), "opening log");
                    return;
                }
            }
        }
        g.started_ms.get_or_insert(ts_ms);
        if let Some(f) = &mut g.file {
            if let Err(e) = writeln!(f, "{line}") {
                tracing::warn!(error = %e, path = %self.path.display(), "appending log");
                g.file = None;
            }
        }
    }
}

fn hours_ms(hours: u32) -> u64 {
    u64::from(hours) * 3_600_000
}

fn first_line_ts(path: &std::path::Path) -> Option<u64> {
    use std::io::{BufRead, BufReader};
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?).read_line(&mut line).ok()?;
    serde_json::from_str::<serde_json::Value>(&line).ok()?.get("ts_ms")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("proxy-rotlog-test-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn lines(p: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn appends_accumulate_and_survive_reopen() {
        let d = dir("append");
        let path = d.join("request-log.jsonl");
        let log = RotatingLog::open(path.clone(), 24);
        log.append(1_000, r#"{"ts_ms":1000,"url":"a"}"#);
        log.append(2_000, r#"{"ts_ms":2000,"url":"b"}"#);
        assert_eq!(lines(&path).len(), 2);
        let log = RotatingLog::open(path.clone(), 24);
        log.append(3_000, r#"{"ts_ms":3000,"url":"c"}"#);
        assert_eq!(lines(&path).len(), 3);
    }

    #[test]
    fn rotation_moves_the_aged_file_aside_and_starts_fresh() {
        let d = dir("rotate");
        let path = d.join("query-log.jsonl");
        let old = d.join("query-log.old.jsonl");
        let log = RotatingLog::open(path.clone(), 1);
        log.append(0, r#"{"ts_ms":0,"q":"first"}"#);
        log.append(3_599_999, r#"{"ts_ms":3599999,"q":"second"}"#);
        assert_eq!(lines(&path).len(), 2);
        assert!(!old.exists());
        log.append(3_600_000, r#"{"ts_ms":3600000,"q":"third"}"#);
        assert_eq!(lines(&path).len(), 1);
        assert_eq!(lines(&old).len(), 2);
        log.append(7_200_001, r#"{"ts_ms":7200001,"q":"fourth"}"#);
        assert_eq!(lines(&old).len(), 1);
        assert!(lines(&old)[0].contains("third"));
    }

    #[test]
    fn restart_rotation_uses_the_first_lines_timestamp() {
        let d = dir("restart");
        let path = d.join("request-log.jsonl");
        std::fs::write(&path, "{\"ts_ms\":1000,\"url\":\"old\"}\n").unwrap();
        let log = RotatingLog::open(path.clone(), 1);
        log.append(3_601_001, r#"{"ts_ms":3601001,"url":"new"}"#);
        assert_eq!(lines(&path).len(), 1, "aged file rotated on first append");
        assert!(lines(&d.join("request-log.old.jsonl"))[0].contains("old"));
    }
}
