//! 剪贴板纯文本读写：兜底粘贴时用。
//!
//! # 为什么只处理文本
//!
//! 图片、文件这类格式是**延迟渲染**的：原主人调用 `SetClipboardData` 时只登记
//! 格式、不给内容，别人 `GetClipboardData` 拿到的是空句柄，内容要由原主人响应
//! `WM_RENDERFORMAT` 现场画出来。我们没法驱动别的进程做这件事 —— 所以还原
//! 得了的只有文本，还原不了的一律如实记日志，不假装成功。
//!
//! # 为什么必须用 Drop 保证关闭
//!
//! `OpenClipboard` 拿到的是**全系统唯一**的一把锁。拿着不还，整台机器的复制
//! 粘贴都会失效，直到我们自己退出为止。这是本模块最严重的失败方式，
//! 所以用 RAII 兜住 —— 包括 panic 展开的路径（release 下是 unwind）。

use anyhow::{anyhow, Result};
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// 剪贴板占用重试：别的进程（剪贴板管理器最常见）正拿着锁时会失败
const OPEN_RETRIES: usize = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(20);

fn format() -> u32 {
    CF_UNICODETEXT.0 as u32
}

/// 保证任何路径都会关掉剪贴板（含 panic 展开）
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open() -> Result<Guard> {
    for attempt in 0..OPEN_RETRIES {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(Guard);
        }
        if attempt + 1 < OPEN_RETRIES {
            // 阻塞上限 80ms。只发生在兜底路径上（会话已到收尾），
            // 而且换来的是"不把整个系统的剪贴板锁死"，值得。
            std::thread::sleep(OPEN_RETRY_DELAY);
        }
    }
    Err(anyhow!("剪贴板正被其他程序占用，暂时写不进去"))
}

/// 剪贴板要的是 UTF-16 且**以 NUL 结尾**，不是 Rust 的 &str
fn utf16_z(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把 `text` 放进剪贴板
pub fn write_text(text: &str) -> Result<()> {
    let units = utf16_z(text);
    let bytes = units.len() * 2;

    let _guard = open()?;
    // 顺序要紧：先备好新内容，**最后**才 EmptyClipboard。反过来的话，
    // 后面任一步失败都会留下"主人的内容已经被清掉、我们又没换上新的"
    // 这种最糟状态 —— 那时我们手里连他原来那份的副本都没有。
    unsafe {
        // 必须 GMEM_MOVEABLE：这块内存要交给内核跨进程搬运，不是我们的堆内存
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let dst = GlobalLock(hglobal);
        if dst.is_null() {
            let _ = GlobalFree(Some(hglobal));
            return Err(anyhow!("剪贴板内存锁定失败"));
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), dst as *mut u16, units.len());
        let _ = GlobalUnlock(hglobal);
        // 到这一步新内容已经就位，紧接着换上：中间没有任何可失败的步骤
        EmptyClipboard()?;
        // 成功之后所有权归系统，不能再 GlobalFree —— 释放它会留下悬空句柄
        if let Err(e) = SetClipboardData(format(), Some(HANDLE(hglobal.0))) {
            let _ = GlobalFree(Some(hglobal));
            return Err(anyhow!("写入剪贴板失败：{e}"));
        }
    }
    Ok(())
}

/// 取回剪贴板里的纯文本。没有文本格式、或读不出来时返回 `None`
/// （主人复制的是图片/文件，或剪贴板正被占用）—— 调用方据此决定"没得还原"。
pub fn read_text() -> Option<String> {
    let _guard = open().ok()?;
    unsafe {
        let handle = GetClipboardData(format()).ok()?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return None;
        }
        // GlobalSize 给的是字节数；长度按"到第一个 NUL 为止"算，
        // 不信任 size（延迟渲染/别人写坏的情况下它未必等于真实长度）
        let cap = GlobalSize(hglobal) / 2;
        let mut len = 0;
        while len < cap && *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

/// 剪贴板序号。主人中途复制了别的东西时它会变 —— 还原之前先核对它，
/// 才不会把等待期间主人新复制的内容覆盖掉。
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// 延迟还原主人原来的剪贴板内容。
///
/// 为什么延迟：粘贴是异步的，目标程序（尤其富格式和远程桌面）可能在
/// `Ctrl+V` 之后才真正去读剪贴板，立刻还原会读到一个空剪贴板。
///
/// 为什么放到独立线程：这里跑在会话的 tokio 工作线程上，睡 300ms 会把
/// 主人紧接着的下一次录音指令堵在队列里。
///
/// `seq` = 我们写完自己那份之后的序列号。
pub fn schedule_restore(previous: String, seq: u32) {
    const RESTORE_DELAY: Duration = Duration::from_millis(300);
    std::thread::spawn(move || {
        std::thread::sleep(RESTORE_DELAY);
        if sequence() != seq {
            // 期间有人写过剪贴板 —— 那是主人刚复制的东西，比我们手里的原文更新
            crate::log::log("剪贴板已被其他程序改写，不还原之前的内容");
            return;
        }
        match write_text(&previous) {
            Ok(()) => crate::log::log("已还原主人原来的剪贴板内容"),
            Err(e) => crate::log::log(format!("还原剪贴板失败：{e}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 剪贴板要的是 UTF-16 且以 NUL 结尾。少了那个 NUL，
    /// 别的程序会把内存里紧跟着的垃圾当正文读出来。
    #[test]
    fn utf16_is_nul_terminated() {
        let units = utf16_z("你");
        assert_eq!(units, vec![0x4F60, 0x0000]);
    }

    #[test]
    fn utf16_keeps_surrogate_pairs_intact() {
        // 𠮷 是代理对，必须原样保留两个编码单元（拆开就是乱码）
        let units = utf16_z("\u{20BB7}");
        assert_eq!(units, vec![0xD842, 0xDFB7, 0x0000]);
    }

    #[test]
    fn empty_text_still_has_a_terminator() {
        assert_eq!(utf16_z(""), vec![0x0000]);
    }

    #[test]
    fn utf16_length_matches_allocated_bytes() {
        let units = utf16_z("今天天气不错。");
        assert_eq!(units.last(), Some(&0), "最后一个单元必须是 NUL");
        assert_eq!(
            units.len() * 2,
            "今天天气不错。".encode_utf16().count() * 2 + 2
        );
    }

    /// 真刀真枪跑一遍 Win32 剪贴板。`GlobalAlloc(GMEM_MOVEABLE)` /
    /// `SetClipboardData` / `GetClipboardData` 这条链路的细节（句柄所有权、
    /// UTF-16、NUL 结尾）靠纯函数是测不出来的 —— 写错一个地方就是一整段
    /// 乱码或者一次访问违例。
    ///
    /// 标了 `#[ignore]`：它会**真的动开发机上的剪贴板**（读完会试着还原），
    /// 所以不进 `cargo test` 的默认集合。需要验证时手动跑：
    /// `cargo test clipboard -- --ignored --nocapture`
    #[test]
    #[ignore = "会动真实剪贴板；需要时手动跑：cargo test clipboard -- --ignored"]
    fn roundtrip_through_the_real_clipboard() {
        // 主人原本复制的是图片/文件时 read_text 拿不到内容（延迟渲染），
        // 那种情况还原不了 —— 这正是"只还原得了文本"这条限制的现场。
        let previous = read_text();
        let probe = "BBVoxi 剪贴板往返测试 🙃";
        write_text(probe).expect("写剪贴板失败");
        assert_eq!(read_text().as_deref(), Some(probe), "读回来的内容对不上");
        if let Some(previous) = previous {
            write_text(&previous).expect("还原原来的剪贴板内容失败");
            assert_eq!(read_text().as_deref(), Some(previous.as_str()));
        }
    }
}
