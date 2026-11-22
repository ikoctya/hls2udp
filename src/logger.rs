use crate::config::get_data_dir;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_RECENT_ENTRIES: usize = 400;
const MAX_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub text: String,
}

struct LogState {
    next_id: u64,
    entries: VecDeque<LogEntry>,
}

static LOGGER: OnceLock<Mutex<LogState>> = OnceLock::new();

fn state() -> &'static Mutex<LogState> {
    LOGGER.get_or_init(|| {
        Mutex::new(LogState {
            next_id: 1,
            entries: VecDeque::with_capacity(MAX_RECENT_ENTRIES + 8),
        })
    })
}

pub fn get_log_dir() -> PathBuf {
    let dir = get_data_dir().join("logs");
    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
    }
    dir
}

pub fn get_log_file_path() -> PathBuf {
    get_log_dir().join("hls2udp_v5.log")
}

pub fn info(msg: impl AsRef<str>) {
    write("INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write("WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write("ERROR", msg.as_ref());
}

fn write(level: &str, msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{}] [{}] {}", ts, level, msg);

    if let Ok(mut guard) = state().lock() {
        let id = guard.next_id;
        guard.next_id += 1;
        guard.entries.push_back(LogEntry {
            id,
            text: line.clone(),
        });
        while guard.entries.len() > MAX_RECENT_ENTRIES {
            guard.entries.pop_front();
        }

        let path = get_log_file_path();
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > MAX_LOG_FILE_BYTES {
                let b1 = path.with_extension("log.1");
                let b2 = path.with_extension("log.2");
                let _ = fs::remove_file(&b2);
                let _ = fs::rename(&b1, &b2);
                let _ = fs::rename(&path, &b1);
            }
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

pub fn write_crash(msg: &str) {
    error(msg);
    let path = get_log_dir().join("crash.log");
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}\n", ts, msg);
    }
}

pub fn get_last_id() -> u64 {
    state().lock().map(|g| g.next_id.saturating_sub(1)).unwrap_or(0)
}

pub fn get_since(since_id: u64, limit: usize) -> Vec<LogEntry> {
    if let Ok(guard) = state().lock() {
        guard
            .entries
            .iter()
            .filter(|e| e.id > since_id)
            .take(limit)
            .cloned()
            .collect()
    } else {
        Vec::new()
    }
}
