//! 极简滚动日志：%APPDATA%\BBVoxi\logs\bbvoxi.log（2MB，另留 2 份旧日志）。
//! ponytail: 不引入 tracing 全家桶，够用到需要结构化日志为止。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
#[cfg(not(windows))]
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
    // 锁中毒（别的线程写日志时 panic）也要继续持有这把锁，
    // 否则后续日志会并发写同一个文件、内容互相穿插。
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        rotate(&path);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", local_timestamp(), msg.as_ref());
    }
    eprintln!("{}", msg.as_ref()); // 开发期同时在控制台可见
}

/// 滚动：bbvoxi.log → bbvoxi.log.1 → bbvoxi.log.2（最老的丢掉），共留 2 份旧日志
fn rotate(path: &std::path::Path) {
    let older = path.with_extension("log.2");
    let _ = fs::remove_file(&older);
    let previous = path.with_extension("log.1");
    let _ = fs::rename(&previous, &older);
    let _ = fs::rename(path, &previous);
}

/// 本地可读时间戳。之前写的是 unix 秒（如 1757491200），排障时完全对不上发生时间。
fn local_timestamp() -> String {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::GetLocalTime;
        let st = unsafe { GetLocalTime() };
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        );
    }
    // 非 Windows 兜底：unix 秒
    #[cfg(not(windows))]
    {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{ts}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 滚动后必须真留两份旧日志（文件头声明的契约），最老的丢掉
    #[test]
    fn rotation_keeps_two_generations() {
        let dir = std::env::temp_dir().join(format!("bbvoxi-log-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bbvoxi.log");

        for gen in ["gen0", "gen1", "gen2"] {
            fs::write(&path, gen).unwrap();
            rotate(&path);
        }

        assert!(!path.exists(), "当前日志应已被移走");
        assert_eq!(fs::read_to_string(path.with_extension("log.1")).unwrap(), "gen2");
        assert_eq!(fs::read_to_string(path.with_extension("log.2")).unwrap(), "gen1");
        let _ = fs::remove_dir_all(&dir);
    }
}
