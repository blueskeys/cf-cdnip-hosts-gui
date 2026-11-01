//! 滚动日志:按天写文件(`logs/YYYY-MM-DD.log`)+ 内存缓冲(UI 日志页)

use crate::config::app_dir;
use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: String,
    pub level: &'static str,
    pub msg: String,
}

struct LoggerInner {
    file_path: PathBuf,
    mem: Vec<LogEntry>,
}

/// 可跨线程克隆的轻量日志器(GUI 线程 / 测速线程 / 写入线程共用)
#[derive(Clone)]
pub struct Logger {
    inner: Arc<Mutex<LoggerInner>>,
}

impl Logger {
    pub fn new() -> Logger {
        let dir = app_dir().join("logs");
        let _ = fs::create_dir_all(&dir);
        let file_path = dir.join(format!("{}.log", Local::now().format("%Y-%m-%d")));
        Logger {
            inner: Arc::new(Mutex::new(LoggerInner { file_path, mem: Vec::new() })),
        }
    }

    pub fn log(&self, level: &'static str, msg: impl Into<String>) {
        let msg = msg.into();
        let mut g = self.inner.lock().unwrap();
        let e = LogEntry {
            time: Local::now().format("%H:%M:%S").to_string(),
            level,
            msg: msg.clone(),
        };
        g.mem.push(e);
        if g.mem.len() > 2000 {
            let drop = g.mem.len() - 2000;
            g.mem.drain(0..drop);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&g.file_path) {
            let _ = writeln!(
                f,
                "{} [{}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                level,
                msg
            );
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.log("INFO", msg);
    }
    pub fn warn(&self, msg: impl Into<String>) {
        self.log("WARN", msg);
    }
    pub fn error(&self, msg: impl Into<String>) {
        self.log("ERROR", msg);
    }

    /// UI 日志页快照
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.inner.lock().unwrap().mem.clone()
    }
}
