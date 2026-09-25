//! 极简滚动日志：%APPDATA%\BBVoxi\logs\bbvoxi.log（2MB，另留 2 份旧日志）。
//! ponytail: 不引入 tracing 全家桶，够用到需要结构化日志为止。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

/// 滚动阈值（不是硬上限）：**达到**这个大小就先把当前日志滚成旧日志，
/// 之后才写这一条。所以单个文件的实际大小最多是 `MAX_BYTES + 一条日志`。
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 写日志的串行锁。注意：它只能保证**本进程内**多个线程不交叉写同一个文件；
/// 多实例（两个 exe 同时跑）并发写同一个日志文件时的先后顺序**不做任何保证**，
/// 内容可能互相穿插。要让多实例也串行需要跨进程锁（命名互斥量），这里不做。
static LOCK: Mutex<()> = Mutex::new(());

/// 重入保护：本进程已经在写日志时再次进入就直接放弃本次写入。
///
/// 为什么必须有它：panic 钩子（见 main.rs）会在 panic 时调 `log()`，而 panic 很可能
/// 就发生在 `log()` 内部（例如下面打开/写文件、写 stderr 出问题）。那一刻我们还
/// **持有**上面那把非重入的 `LOCK`，同线程二次加锁会自死锁 —— 界面永久冻结，连
/// 崩溃日志都写不出来。宁可丢一条日志也绝不能锁死。
/// 代价：别的线程正好在写日志时，本线程这一条会被丢掉（日志量很低，可以接受）。
static IN_LOG: AtomicBool = AtomicBool::new(false);

/// 上一次「滚动失败」的时刻（毫秒，见 `now_ms`）；0 = 从未失败过。
static LAST_ROTATE_FAIL: AtomicU64 = AtomicU64::new(0);

/// 滚动失败后的冷却时间：这么久之内不再重试滚动，见 `rotate_if_due`。
const ROTATE_BACKOFF_MS: u64 = 60_000;

fn log_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("BBVoxi")
            .join("logs")
            .join("bbvoxi.log"),
    )
}

/// 在 `log()` 的任意出口（含提前 return）自动清掉重入标记
struct InLogGuard;

impl Drop for InLogGuard {
    fn drop(&mut self) {
        IN_LOG.store(false, Ordering::SeqCst);
    }
}

pub fn log(msg: impl AsRef<str>) {
    // 重入保护：已经在写日志就直接返回，绝不二次加锁（见 `IN_LOG` 的说明）
    if IN_LOG.swap(true, Ordering::SeqCst) {
        return;
    }
    let _reset = InLogGuard;

    let Some(path) = log_path() else { return };
    // 锁中毒（别的线程写日志时 panic）也要继续持有这把锁，
    // 否则后续日志会并发写同一个文件、内容互相穿插。
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // 用 `>=`：大小恰好等于阈值时也要滚。若用 `>`，这一次先写进去，文件就变成
    // 「阈值 + 一条日志」，文件头声明的上限成了空话，且每次都要多写一条才滚。
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= MAX_BYTES {
        rotate_if_due(&path);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", local_timestamp(), msg.as_ref());
    }
    // 开发期同时在控制台可见。这里**不能**用 `eprintln!`：stderr 不可写（被重定向到
    // 已关闭的管道等）时它会 panic，而那一刻我们还持有上面那把非重入的锁 ——
    // panic 钩子再调 `log()` 就是同线程二次加锁、永久死锁。写失败的 Err 直接丢掉。
    let _ = writeln!(std::io::stderr(), "{}", msg.as_ref());
}

/// 到点就滚动，失败则退避。
///
/// 为什么要退避：滚动要动三个文件名，任何一步失败（例如 log.1 被杀软/编辑器占用）
/// 都会让整个滚动失败；若不做退避，之后**每写一条日志**都会把这失败的三步重试
/// 一遍 —— 文件无上限增长、旧日志被反复清掉，而且完全无声。
/// 失败时写一条「滚动失败」日志并记下时间，冷却期内直接跳过滚动。
fn rotate_if_due(path: &std::path::Path) {
    let now = now_ms();
    let last_fail = LAST_ROTATE_FAIL.load(Ordering::Relaxed);
    if last_fail != 0 && now.saturating_sub(last_fail) < ROTATE_BACKOFF_MS {
        return; // 冷却期内：暂时让日志超过阈值，好过每秒重试失败的三步
    }
    match rotate(path) {
        Ok(()) => LAST_ROTATE_FAIL.store(0, Ordering::Relaxed),
        Err(e) => {
            LAST_ROTATE_FAIL.store(now, Ordering::Relaxed);
            note_rotate_failure(path, &e);
        }
    }
}

/// 滚动失败时把原因记进日志。
///
/// 不能用 `log()`：那会二次获取 `IN_LOG`／`LOCK`（我们此刻正持有），要么被重入保护
/// 丢掉、要么自死锁。既然本来就握着写日志的路径，直接往同一个文件追加一行。
fn note_rotate_failure(path: &std::path::Path, err: &std::io::Error) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            f,
            "[{}] 日志滚动失败（{}），{} 秒内不再尝试滚动：{}",
            local_timestamp(),
            err,
            ROTATE_BACKOFF_MS / 1000,
            path.display()
        );
    }
}

/// 滚动：bbvoxi.log → bbvoxi.log.1 → bbvoxi.log.2（最老的丢掉），共留 2 份旧日志。
///
/// 任一步失败都如实返回错误，交给 `rotate_if_due` 报警 + 退避；**不能**像以前那样
/// 用 `let _ =` 把三步的错误全丢掉。
fn rotate(path: &std::path::Path) -> std::io::Result<()> {
    let older = path.with_extension("log.2");
    // 最老的那份删不掉不算错：第一次滚动时它本来就不存在
    match fs::remove_file(&older) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let previous = path.with_extension("log.1");
    match fs::rename(&previous, &older) {
        Ok(()) => {}
        // 第一次滚动时 log.1 还不存在，忽略掉
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::rename(path, &previous)
}

/// 单调递增的毫秒数（Windows 上取系统开机时长，不受主人改系统时间影响）
fn now_ms() -> u64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::GetTickCount64;
        return unsafe { GetTickCount64() };
    }
    #[cfg(not(windows))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
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
            rotate(&path).unwrap();
        }

        assert!(!path.exists(), "当前日志应已被移走");
        assert_eq!(
            fs::read_to_string(path.with_extension("log.1")).unwrap(),
            "gen2"
        );
        assert_eq!(
            fs::read_to_string(path.with_extension("log.2")).unwrap(),
            "gen1"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 滚动失败必须如实报错、并进入退避。
    ///
    /// 这条守的是"失败后不要每写一条日志就重试失败的三步"：以前三步的错误全被
    /// `let _ =` 丢掉，一旦 log.1 被占用，文件就无上限增长、旧日志被反复清掉、
    /// 还完全无声。这里先把一个**目录**放在 log.2 的位置，`remove_file` 必定失败。
    #[test]
    fn rotate_failure_sets_backoff() {
        let dir = std::env::temp_dir().join(format!("bbvoxi-log-backoff-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bbvoxi.log");
        fs::write(&path, "x").unwrap();
        fs::create_dir(path.with_extension("log.2")).unwrap();

        assert!(rotate(&path).is_err(), "remove_file 删不掉目录时必须报错");
        rotate_if_due(&path);
        assert_ne!(
            LAST_ROTATE_FAIL.load(Ordering::Relaxed),
            0,
            "失败后必须记下时间戳，才能在冷却期内跳过重试"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
