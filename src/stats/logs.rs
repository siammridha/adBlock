//! Rotating log file: one current file plus one old file.
//!
//! Writes never touch the disk on the caller's thread. `append` hands the line
//! to a dedicated writer thread over a channel and returns immediately, so the
//! proxy's request path never blocks on (possibly slow) file I/O. Reads share a
//! mutex with the writer so a page or lookup can't observe a half-done
//! rotation.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub(super) struct RotatingLog {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
}

/// State the writer thread and the readers share. The mutex guards the open
/// file and rotation bookkeeping; the paths never change after construction.
struct Shared {
    path: PathBuf,
    old_path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    started_ms: Option<u64>,
    rotate_ms: u64,
}

enum Cmd {
    /// A line to append, tagged with its timestamp for rotation timing.
    Line { ts_ms: u64, line: String },
    /// Barrier: the writer drains everything queued before it, then acks. Only
    /// tests construct this (to wait for the disk); the writer still handles it.
    #[cfg_attr(not(test), allow(dead_code))]
    Flush(Sender<()>),
}

impl RotatingLog {
    pub(super) fn open(path: PathBuf, rotate_hours: u32) -> Self {
        let old_path = path.with_extension("old.jsonl");
        let started_ms = first_line_ts(&path);
        let shared = Arc::new(Shared {
            path,
            old_path,
            inner: Mutex::new(Inner {
                file: None,
                started_ms,
                rotate_ms: hours_ms(rotate_hours),
            }),
        });
        let (tx, rx) = mpsc::channel();
        let writer = Arc::clone(&shared);
        // A named OS thread owns the actual writing. It lives until the channel
        // closes (when this RotatingLog drops).
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || writer_loop(&writer, rx))
            .expect("spawn log writer thread");
        Self { shared, tx }
    }

    pub(super) fn set_rotate_hours(&self, hours: u32) {
        self.shared.inner.lock().expect("log lock").rotate_ms = hours_ms(hours);
    }

    /// Queue a line for the writer thread. Non-blocking: the disk write happens
    /// off this thread. A dropped send (writer gone) silently discards the line.
    pub(super) fn append(&self, ts_ms: u64, line: &str) {
        let _ = self.tx.send(Cmd::Line { ts_ms, line: line.to_string() });
    }

    /// Block until the writer has flushed everything queued so far. After this
    /// returns, all earlier `append`s are on disk. Used by tests; production
    /// reads tolerate the tiny lag and don't call it.
    #[cfg(test)]
    pub(super) fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Cmd::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Up to `limit` lines, newest first, whose `seq_of(line)` is below
    /// `before` (all of them when `before` is `None`). Reads the current file
    /// then the rotated-out file, each back to front. Lines that fail
    /// `seq_of` — blanks, or older lines predating the field — are skipped.
    pub(super) fn page(
        &self,
        before: Option<u64>,
        limit: usize,
        seq_of: impl Fn(&str) -> Option<u64>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        if limit == 0 {
            return out;
        }
        // Hold the lock so a concurrent rotation can't rename the files mid-read.
        let _g = self.shared.inner.lock().expect("log lock");
        for path in [&self.shared.path, &self.shared.old_path] {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for line in text.lines().rev() {
                let Some(seq) = seq_of(line) else { continue };
                if before.is_some_and(|b| seq >= b) {
                    continue;
                }
                out.push(line.to_string());
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    /// The newest line matching `pred`, or `None`. Scans current then rotated
    /// file, back to front, so the most recent match wins.
    pub(super) fn find(&self, pred: impl Fn(&str) -> bool) -> Option<String> {
        let _g = self.shared.inner.lock().expect("log lock");
        for path in [&self.shared.path, &self.shared.old_path] {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if let Some(line) = text.lines().rev().find(|l| pred(l)) {
                return Some(line.to_string());
            }
        }
        None
    }
}

fn writer_loop(shared: &Shared, rx: Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Line { ts_ms, line } => shared.write_line(ts_ms, &line),
            Cmd::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

impl Shared {
    fn write_line(&self, ts_ms: u64, line: &str) {
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
        log.flush();
        assert_eq!(lines(&path).len(), 2);
        drop(log);
        let log = RotatingLog::open(path.clone(), 24);
        log.append(3_000, r#"{"ts_ms":3000,"url":"c"}"#);
        log.flush();
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
        log.flush();
        assert_eq!(lines(&path).len(), 2);
        assert!(!old.exists());
        log.append(3_600_000, r#"{"ts_ms":3600000,"q":"third"}"#);
        log.flush();
        assert_eq!(lines(&path).len(), 1);
        assert_eq!(lines(&old).len(), 2);
        log.append(7_200_001, r#"{"ts_ms":7200001,"q":"fourth"}"#);
        log.flush();
        assert_eq!(lines(&old).len(), 1);
        assert!(lines(&old)[0].contains("third"));
    }

    fn seq_of(line: &str) -> Option<u64> {
        serde_json::from_str::<serde_json::Value>(line).ok()?.get("seq")?.as_u64()
    }

    #[test]
    fn page_walks_newest_first_across_both_files_honoring_the_cursor() {
        let d = dir("page");
        let path = d.join("request-log.jsonl");
        let log = RotatingLog::open(path.clone(), 1);
        // seq 1,2 land, then rotation pushes them aside; seq 3,4 are current.
        log.append(0, r#"{"seq":1,"ts_ms":0}"#);
        log.append(1, r#"{"seq":2,"ts_ms":1}"#);
        log.append(3_600_000, r#"{"seq":3,"ts_ms":3600000}"#);
        log.append(3_600_001, r#"{"seq":4,"ts_ms":3600001}"#);
        log.flush();

        // Newest first, spanning current then the rotated-out file.
        let all = log.page(None, 10, seq_of);
        let seqs: Vec<u64> = all.iter().filter_map(|l| seq_of(l)).collect();
        assert_eq!(seqs, vec![4, 3, 2, 1]);

        // Limit stops the walk early.
        assert_eq!(log.page(None, 2, seq_of).len(), 2);

        // The cursor excludes seqs at or above it.
        let older = log.page(Some(3), 10, seq_of);
        assert_eq!(older.iter().filter_map(|l| seq_of(l)).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn find_returns_the_newest_matching_line() {
        let d = dir("find");
        let path = d.join("request-detail.jsonl");
        let log = RotatingLog::open(path.clone(), 24);
        log.append(0, r#"{"seq":1,"resp_body":"a"}"#);
        log.append(1, r#"{"seq":2,"resp_body":"b"}"#);
        log.flush();
        let hit = log.find(|l| seq_of(l) == Some(2)).unwrap();
        assert!(hit.contains("\"b\""));
        assert!(log.find(|l| seq_of(l) == Some(99)).is_none());
    }

    #[test]
    fn restart_rotation_uses_the_first_lines_timestamp() {
        let d = dir("restart");
        let path = d.join("request-log.jsonl");
        std::fs::write(&path, "{\"ts_ms\":1000,\"url\":\"old\"}\n").unwrap();
        let log = RotatingLog::open(path.clone(), 1);
        log.append(3_601_001, r#"{"ts_ms":3601001,"url":"new"}"#);
        log.flush();
        assert_eq!(lines(&path).len(), 1, "aged file rotated on first append");
        assert!(lines(&d.join("request-log.old.jsonl"))[0].contains("old"));
    }
}
