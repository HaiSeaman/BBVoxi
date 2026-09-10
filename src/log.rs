//! 极简滚动日志：%APPDATA%\BBVoxi\logs\bbvoxi.log（2MB × 2 份）。
//! ponytail: 不引入 tracing 全家桶，够用到需要结构化日志为止。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

static LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("BBVoxi")
            .join("logs")
            .join("bbvoxi.log"),
    )
}

pub fn log(msg: impl AsRef<str>) {
    let Some(path) = log_path() else { return };
    let _guard = LOCK.lock();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{ts}] {}", msg.as_ref());
    }
    eprintln!("{}", msg.as_ref()); // 开发期同时在控制台可见
}
