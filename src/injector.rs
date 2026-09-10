//! 文字注入：SendInput + KEYEVENTF_UNICODE 逐字符模拟打字。
//! 不碰剪贴板，因此不会破坏用户正在复制的内容。

use anyhow::{anyhow, Result};
use std::mem::size_of;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_RETURN,
    VK_TAB,
};

/// 一次 SendInput 提交的字符数（每个字符 2 个事件）
const BATCH_CHARS: usize = 16;
const ERROR_ACCESS_DENIED: u32 = 5;

/// 我们注入的事件都带这个标记（dwExtraInfo）。
/// 键盘钩子据此识别"这是自己发的"，避免把自己的修饰键重置误当成用户按键。
pub const INJECT_TAG: usize = 0x4242_564F_5849; // "BBVOXI"

/// 把修饰键重置事件追加到批次开头
fn push_modifier_reset(buf: &mut Vec<INPUT>) {
    for (vk, _, extended) in crate::hotkey::MODIFIERS {
        let mut flags = KEYEVENTF_KEYUP;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        buf.push(keyboard_input(vk, 0, flags));
    }
}

pub fn type_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut buf: Vec<INPUT> = Vec::with_capacity(BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len());
    push_modifier_reset(&mut buf);
    for unit in text.encode_utf16() {
        match unit {
            0x0A => {
                push_key(&mut buf, VK_RETURN, false);
                push_key(&mut buf, VK_RETURN, true);
            }
            0x0D => continue, // \r\n 里只处理 \n
            0x09 => {
                push_key(&mut buf, VK_TAB, false);
                push_key(&mut buf, VK_TAB, true);
            }
            _ => {
                // UTF-16 编码单元直接送，代理对天然被拆成两次，生僻字/emoji 也能输入
                push_unicode(&mut buf, unit, false);
                push_unicode(&mut buf, unit, true);
            }
        }
        if buf.len() >= BATCH_CHARS * 2 {
            flush(&mut buf)?;
        }
    }
    flush(&mut buf)
}

/// 退格删除：实时输入时用来回退被识别修正的文字
pub fn backspace(count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let mut buf: Vec<INPUT> = Vec::with_capacity(BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len());
    push_modifier_reset(&mut buf);
    for _ in 0..count {
        push_key(&mut buf, VK_BACK, false);
        push_key(&mut buf, VK_BACK, true);
        if buf.len() >= BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len() {
            flush(&mut buf)?;
        }
    }
    flush(&mut buf)
}

fn push_unicode(buf: &mut Vec<INPUT>, scan: u16, keyup: bool) {
    let mut flags = KEYEVENTF_UNICODE;
    if keyup {
        flags |= KEYEVENTF_KEYUP;
    }
    buf.push(keyboard_input(VIRTUAL_KEY(0), scan, flags));
}

fn push_key(buf: &mut Vec<INPUT>, vk: VIRTUAL_KEY, keyup: bool) {
    let flags = if keyup {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    buf.push(keyboard_input(vk, 0, flags));
}

fn keyboard_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn flush(buf: &mut Vec<INPUT>) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let expected = buf.len() as u32;
    let sent = unsafe { SendInput(buf, size_of::<INPUT>() as i32) };
    buf.clear();
    if sent != expected {
        let err = unsafe { GetLastError() };
        if err.0 == ERROR_ACCESS_DENIED {
            return Err(anyhow!(
                "目标程序以管理员权限运行，BBVoxi 无法向它输入文字（请改用普通权限的窗口）"
            ));
        }
        return Err(anyhow!("模拟键盘输入失败（错误码 {}）", err.0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(input: &INPUT) -> (u16, u32) {
        let ki = unsafe { input.Anonymous.ki };
        (ki.wVk.0, ki.dwFlags.0)
    }

    /// 左右修饰键都要重置，右键还要带扩展键标记（只发 VK_CONTROL 是无效的）
    #[test]
    fn modifier_reset_covers_both_sides_with_extended_flags() {
        let mut buf = Vec::new();
        push_modifier_reset(&mut buf);
        assert_eq!(buf.len(), crate::hotkey::MODIFIERS.len());

        let keys: Vec<(u16, u32)> = buf.iter().map(key_of).collect();
        for (vk, _kind, extended) in crate::hotkey::MODIFIERS {
            let want = KEYEVENTF_KEYUP.0 | if extended { KEYEVENTF_EXTENDEDKEY.0 } else { 0 };
            assert!(
                keys.contains(&(vk.0, want)),
                "缺少 {vk:?} 的重置事件（扩展键={extended}）"
            );
        }
        assert!(
            keys.iter().all(|(_, f)| f & KEYEVENTF_KEYUP.0 != 0),
            "重置必须是 KEYUP"
        );
        // 左右变体都必须在表里（漏掉左 Ctrl 正是之前满屏 1111 的原因）
        for (vk, kind) in [
            (0xA2u16, crate::hotkey::ModKind::Ctrl),
            (0xA3, crate::hotkey::ModKind::Ctrl),
            (0xA4, crate::hotkey::ModKind::Alt),
            (0xA5, crate::hotkey::ModKind::Alt),
            (0xA0, crate::hotkey::ModKind::Shift),
            (0xA1, crate::hotkey::ModKind::Shift),
        ] {
            assert!(
                crate::hotkey::MODIFIERS
                    .iter()
                    .any(|(k, kd, _)| k.0 == vk && *kd == kind),
                "修饰键表缺少 0x{vk:X}"
            );
        }
    }

    /// 关键约束：重置事件必须与文本在同一个批次里，否则修饰键消息会插回队列
    #[test]
    fn text_batch_starts_with_modifier_reset() {
        let mut buf = Vec::new();
        push_modifier_reset(&mut buf);
        push_unicode(&mut buf, '你' as u16, false);
        push_unicode(&mut buf, '你' as u16, true);

        assert_eq!(buf.len(), crate::hotkey::MODIFIERS.len() + 2);
        let (vk, flags) = key_of(&buf[crate::hotkey::MODIFIERS.len()]);
        assert_eq!(vk, 0, "unicode 事件不应带虚拟键码");
        assert_eq!(flags & KEYEVENTF_UNICODE.0, KEYEVENTF_UNICODE.0);
        assert_eq!(
            unsafe { buf[crate::hotkey::MODIFIERS.len()].Anonymous.ki.wScan },
            0x4F60
        );
    }

    /// 我们注入的每个事件都要带标记，钩子据此识别"自己发的"
    #[test]
    fn injected_events_carry_our_tag() {
        let mut buf = Vec::new();
        push_modifier_reset(&mut buf);
        push_key(&mut buf, VK_BACK, false);
        push_unicode(&mut buf, 0x4F60, false);
        assert!(
            buf.iter()
                .all(|i| unsafe { i.Anonymous.ki.dwExtraInfo } == INJECT_TAG),
            "注入事件缺少标记，钩子将无法识别"
        );
    }

    #[test]
    fn surrogate_pairs_are_split_into_two_events_each() {
        // "𠮷" 是一个代理对，编码后 2 个 u16 → 每个 2 个事件，共 4 个
        let mut buf = Vec::new();
        for unit in "\u{20BB7}".encode_utf16() {
            push_unicode(&mut buf, unit, false);
            push_unicode(&mut buf, unit, true);
        }
        assert_eq!(buf.len(), 4);
        let codes: Vec<u16> = buf
            .iter()
            .map(|i| unsafe { i.Anonymous.ki.wScan })
            .collect();
        assert_eq!(codes, vec![0xD842, 0xD842, 0xDFB7, 0xDFB7]);
    }

    #[test]
    fn newline_maps_to_enter_keycode() {
        let mut buf = Vec::new();
        push_key(&mut buf, VK_RETURN, false);
        assert_eq!(unsafe { buf[0].Anonymous.ki.wVk }, VK_RETURN);
        assert_eq!(unsafe { buf[0].Anonymous.ki.wScan }, 0);
    }
}
