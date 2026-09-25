//! 文字注入：SendInput + KEYEVENTF_UNICODE 逐字符模拟打字。
//!
//! 首选路径不碰剪贴板，因此不会破坏用户正在复制的内容。
//! 逐字注入被目标程序拒掉时，由 `typer` 改用 [`paste`] 走剪贴板兜底 ——
//! 两条通道在 Windows 里是**不同**的路径，认后者的程序并不少。
//! 本模块只负责按键合成，剪贴板的读写都在 `clipboard` 里。

use anyhow::{anyhow, Result};
use std::mem::size_of;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    VK_BACK, VK_LCONTROL, VK_RETURN, VK_TAB, VK_V,
};

/// 一次 SendInput 提交的字符数（每个字符 2 个事件）
const BATCH_CHARS: usize = 16;
const ERROR_ACCESS_DENIED: u32 = 5;

/// 我们注入的事件都带这个标记（dwExtraInfo）。
/// 键盘钩子据此识别"这是自己发的"，避免把自己的修饰键重置误当成用户按键。
pub const INJECT_TAG: usize = 0x4242_564F_5849; // "BBVOXI"

/// 目标窗口以管理员权限运行 —— Windows 的 UIPI 会拦下我们**整个** `SendInput`。
///
/// 单独立一个类型、而不是只留一句错误消息，是为了让上层能分辨出它：
/// 这种情况下 `Ctrl+V` 走的同样是 `SendInput`，一样会被拦，再试一次粘贴
/// 只会白等 80ms 并多报一次错。
#[derive(Debug)]
pub struct AccessDenied;

impl std::fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "目标程序以管理员权限运行，BBVoxi 无法向它输入文字（请改用普通权限的窗口）"
        )
    }
}

impl std::error::Error for AccessDenied {}

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

/// 逐字注入。返回值 = **实际提交成功的 UTF-16 编码单元数**（`\r` 不计入）。
///
/// 为什么不只返回 Ok/Err：`SendInput` 只保证"事件进了系统队列"，目标程序
/// 完全可能只收下一部分（被 UIPI 拦、队列满、程序自己吞掉）。上层 `typer`
/// 必须知道这段文字到底进去多少 —— 没进去的尾巴才有机会改走剪贴板补上；
/// 只凭 Ok 就当整段成功，会让主人眼看着"打出去了"其实丢了半句。
pub fn type_text(text: &str) -> Result<usize> {
    if text.is_empty() {
        return Ok(0);
    }
    let mut buf: Vec<INPUT> = Vec::with_capacity(BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len());
    push_modifier_reset(&mut buf);
    // 修饰键重置只出现在第一个批次里，换算"提交了几个字符"时要先把它扣掉
    let mut header = crate::hotkey::MODIFIERS.len();
    let mut committed = 0usize; // 已确认提交成功的编码单元数
    let mut batch_units = 0usize; // 当前批次里已排入的编码单元数
    for unit in text.encode_utf16() {
        match unit {
            // 回车/Tab 走命名键，但必须带真实扫描码：只读扫描码的程序
            // （游戏、部分远程桌面）看不见 wScan=0 的键，会出现"字打了、换行却没反应"。
            0x0A => push_key_stroke(&mut buf, VK_RETURN),
            0x0D => continue, // \r\n 里只处理 \n
            0x09 => push_key_stroke(&mut buf, VK_TAB),
            _ => {
                // UTF-16 编码单元直接送，代理对天然被拆成两次，生僻字/emoji 也能输入
                push_unicode(&mut buf, unit, false);
                push_unicode(&mut buf, unit, true);
            }
        }
        batch_units += 1;
        if buf.len() >= BATCH_CHARS * 2 {
            committed += flush_batch(&mut buf, batch_units, header)?;
            header = 0;
            batch_units = 0;
        }
    }
    committed += flush_batch(&mut buf, batch_units, header)?;
    Ok(committed)
}

/// `type_text` 的提交计数口径：UTF-16 编码单元里除去 `\r` 的数量。
/// 调用方拿它当"期望值"，和 `type_text` 的返回值比对，才知道有没有打全。
pub fn typeable_units(text: &str) -> usize {
    text.encode_utf16().filter(|u| *u != 0x0D).count()
}

/// 提交一个批次，并把"内核实际接受的事件数"换算成**编码单元数**返回。
fn flush_batch(buf: &mut Vec<INPUT>, units: usize, header: usize) -> Result<usize> {
    let expected = buf.len();
    let sent = flush(buf)?;
    if sent >= expected {
        return Ok(units);
    }
    // 只进去一部分：每单元 2 个事件，先扣掉不占字符的修饰键重置头。
    // 返回真实提交数，让上层只对没进去的那段尾巴做兜底，避免重复粘贴。
    Ok(sent.saturating_sub(header) / 2)
}

/// 剪贴板粘贴：注入一次 `Ctrl+V`。
///
/// 为什么值得单独做一条通道：`KEYEVENTF_UNICODE` 和 `Ctrl+V` 在 Windows 里走的是
/// 两条不同的路径 —— 忽略前者的程序（远程桌面、部分 Electron/Java/老 MFC）
/// 认后者，所以它是逐字注入被拒之后唯一有意义的备选。
///
/// 三处与 `type_text` 不同，都不是随手写的：
/// - 这里要**按下** Ctrl。逐字注入反而是先把修饰键清掉（按住快捷键说话时
///   Ctrl 一直按着，不清就会变成 Ctrl+字符）；
/// - 必须带**扫描码**。有些程序只读硬件的扫描码，`wScan` 为 0 的按键它们
///   根本看不见 —— 而那正是需要走粘贴兜底的那类目标；
/// - 末尾再清一次修饰键。目标程序若在粘贴过程中吞掉了某个 KEYUP，Ctrl 会卡住，
///   之后主人敲的每个键都会变成 `Ctrl+键`。`push_modifier_reset` 覆盖了全部左右变体，
///   重复一次是幂等的。
pub fn paste() -> Result<()> {
    let mut buf: Vec<INPUT> = Vec::with_capacity(crate::hotkey::MODIFIERS.len() * 2 + 5);
    paste_events(&mut buf);
    // 粘贴是一串有先后的按键（Ctrl↓ V↓ V↑ Ctrl↑），只进去一半会留下卡住的 Ctrl，
    // 所以这里要求整批都必须成功，不能用"部分成功也算过"的宽松口径。
    flush_all(&mut buf)
}

/// 组装一次粘贴的按键序列（与 `paste` 分开，便于单测校验顺序与扫描码）
fn paste_events(buf: &mut Vec<INPUT>) {
    let (ctrl, ctrl_scan) = (VK_LCONTROL, scan_code(VK_LCONTROL));
    let v_scan = scan_code(VK_V);
    push_modifier_reset(buf);
    push_key(buf, ctrl, ctrl_scan, false);
    push_key(buf, VK_V, v_scan, false);
    push_key(buf, VK_V, v_scan, true);
    push_key(buf, ctrl, ctrl_scan, true);
    push_modifier_reset(buf);
}

/// 虚拟键码 → 扫描码。查不到时返回 0，此时事件依然合法
/// （与逐字注入的做法一致），只是读扫描码的程序看不到它。
/// 这里用的 `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)` 就是"查不到时"的兜底来源：
/// 它是 Windows 自己给的映射表，比我们手写一张表更靠谱。
fn scan_code(vk: VIRTUAL_KEY) -> u16 {
    unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) as u16 }
}

/// 追加一次"按下+松开"，扫描码统一由 `scan_code` 取真实值。
///
/// 为什么专门抽出来：退格/回车/Tab 这类命名键以前传的是 `wScan = 0`，
/// 而只读扫描码的程序（游戏、部分远程桌面）根本看不见它们 ——
/// 会出现"字打出去了、退格却删不掉，回车也不换行"。这里和 `paste_events`
/// 用同一套真实扫描码，两条路径的口径就一致了。
fn push_key_stroke(buf: &mut Vec<INPUT>, vk: VIRTUAL_KEY) {
    let scan = scan_code(vk);
    push_key(buf, vk, scan, false);
    push_key(buf, vk, scan, true);
}

/// 补一次完整的 Win 按下+松开。
///
/// 用途：组合里带 Win 时，Win 的按下由键盘钩子接管（见 `hotkey::Hold`），
/// 当主人**只是想按 Win 打开开始菜单**时，欠他的那次按键在这里还回去。
///
/// 为什么不干脆放行原始按下：
/// - 放行了它就再也擦不掉 —— 松开时外壳必弹开始菜单抢走焦点，正在说话的主人
///   这次识别结果一个字都打不进目标程序；
/// - 吞掉松开也不行 —— 外壳会以为 Win 一直按着，之后主人随便敲个字母都会触发
///   Win+X（打开资源管理器）。
///
/// 这两条都在真机上用注入探针确认过，而"补一整套按下+松开"实测能让开始菜单
/// 照常弹出、且系统里 Win 的状态是干净的。
///
/// Win 是扩展键，按下与松开都必须带 `KEYEVENTF_EXTENDEDKEY`
/// （与 `hotkey::MODIFIERS` 表里的标注一致）。
pub fn press_win(win: u32) -> Result<()> {
    let mut buf: Vec<INPUT> = Vec::with_capacity(2);
    win_press_events(&mut buf, win);
    flush_all(&mut buf)
}

/// 组装「补一次完整 Win 按键」的事件序列（与 `press_win` 分开，便于单测校验：
/// 单测里绝不能真的注入一次 Win 按下+松开 —— 那会当场弹出开始菜单）
fn win_press_events(buf: &mut Vec<INPUT>, win: u32) {
    push_win(buf, win, false);
    push_win(buf, win, true);
}

/// 一次提交「Win 按下 + 某个键按下」：主人按的是 Win+E 这类系统组合键。
///
/// 为什么**必须同一次 `SendInput`**：外壳判断 Win 组合键靠的是"收到这个键时
/// Win 是否已经按下"。分两次提交、或先把当前键放行再补 Win，外壳都会先收到
/// 那个键（那时它以为 Win 没按下），组合键失效、字母被当普通字符打出去 ——
/// 这三种顺序都拿注入探针实测过，只有同批次提交才生效。
///
/// 当前键的虚拟键码与扫描码都照原样带上：只读扫描码的程序（游戏、部分远程桌面）
/// 才认得出来（与 `push_key_stroke`、`paste_events` 同一口径）。
pub fn win_then_key(win: u32, key_vk: u32, scan: u16, extended: bool) -> Result<()> {
    let mut buf: Vec<INPUT> = Vec::with_capacity(2);
    win_then_key_events(&mut buf, win, key_vk, scan, extended);
    flush_all(&mut buf)
}

/// 组装「Win 按下 + 当前键按下」的事件序列（与 `win_then_key` 分开，便于单测）
fn win_then_key_events(buf: &mut Vec<INPUT>, win: u32, key_vk: u32, scan: u16, extended: bool) {
    push_win(buf, win, false);
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    buf.push(keyboard_input(VIRTUAL_KEY(key_vk as u16), scan, flags));
}

/// Win 键的按下/松开。扩展键标记必须带，否则外壳认不出这是一次 Win 按键。
fn push_win(buf: &mut Vec<INPUT>, win: u32, keyup: bool) {
    let mut flags = KEYEVENTF_EXTENDEDKEY;
    if keyup {
        flags |= KEYEVENTF_KEYUP;
    }
    buf.push(keyboard_input(VIRTUAL_KEY(win as u16), 0, flags));
}

/// 退格删除：实时输入时用来回退被识别修正的文字
pub fn backspace(count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let mut buf: Vec<INPUT> = Vec::with_capacity(BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len());
    push_modifier_reset(&mut buf);
    for _ in 0..count {
        push_key_stroke(&mut buf, VK_BACK);
        if buf.len() >= BATCH_CHARS * 2 + crate::hotkey::MODIFIERS.len() {
            flush_all(&mut buf)?;
        }
    }
    flush_all(&mut buf)
}

fn push_unicode(buf: &mut Vec<INPUT>, scan: u16, keyup: bool) {
    let mut flags = KEYEVENTF_UNICODE;
    if keyup {
        flags |= KEYEVENTF_KEYUP;
    }
    buf.push(keyboard_input(VIRTUAL_KEY(0), scan, flags));
}

/// `scan` = 硬件扫描码，读扫描码的程序（游戏、部分远程桌面）只认它；
/// 不需要时传 0 即可。
fn push_key(buf: &mut Vec<INPUT>, vk: VIRTUAL_KEY, scan: u16, keyup: bool) {
    let flags = if keyup {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    buf.push(keyboard_input(vk, scan, flags));
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

/// 把一批事件交给系统，返回**内核实际接受的事件数**。
///
/// 一个都没进去时按错误抛出（含 UIPI 拦截）；只进去一部分时返回实际数量，
/// 由调用方自己决定怎么办 —— 对逐字注入来说"少打几个字"不算整批失败，
/// 可以把没进去的尾巴改走剪贴板；对粘贴/退格这类按键序列则不能将就。
fn flush(buf: &mut Vec<INPUT>) -> Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let expected = buf.len() as u32;
    let sent = unsafe { SendInput(buf, size_of::<INPUT>() as i32) };
    buf.clear();
    if sent == expected {
        return Ok(sent as usize);
    }
    let err = unsafe { GetLastError() };
    if sent == 0 || err.0 == ERROR_ACCESS_DENIED {
        if err.0 == ERROR_ACCESS_DENIED {
            return Err(anyhow::Error::new(AccessDenied));
        }
        return Err(anyhow!("模拟键盘输入失败（错误码 {}）", err.0));
    }
    Ok(sent as usize)
}

/// 要求整批都必须提交成功；只进去一部分就按失败报出来。
fn flush_all(buf: &mut Vec<INPUT>) -> Result<()> {
    let expected = buf.len();
    let sent = flush(buf)?;
    if sent != expected {
        return Err(anyhow!("模拟键盘输入只提交了 {sent}/{expected} 个事件"));
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
        push_key(&mut buf, VK_BACK, 0, false);
        push_unicode(&mut buf, 0x4F60, false);
        assert!(
            buf.iter()
                .all(|i| unsafe { i.Anonymous.ki.dwExtraInfo } == INJECT_TAG),
            "注入事件缺少标记，钩子将无法识别"
        );
    }

    /// 粘贴是一条独立的注入批次，同样必须带标记 —— 否则钩子会把它
    /// 当成主人的按键，我们注入的 Ctrl+V 就会去触发主人自己的快捷键
    #[test]
    fn paste_events_carry_our_tag() {
        let mut buf = Vec::new();
        paste_events(&mut buf);
        assert!(
            buf.iter()
                .all(|i| unsafe { i.Anonymous.ki.dwExtraInfo } == INJECT_TAG),
            "粘贴事件缺少标记"
        );
    }

    /// 读扫描码的程序（游戏、部分远程桌面）看不到 `wScan` 为 0 的按键 ——
    /// 而那正是需要靠粘贴兜底救回来的那类目标，所以这里的扫描码不能是 0
    #[test]
    fn paste_carries_real_scan_codes() {
        assert_ne!(scan_code(VK_V), 0, "V 的扫描码不该是 0");
        assert_ne!(scan_code(VK_LCONTROL), 0, "左 Ctrl 的扫描码不该是 0");

        let mut buf = Vec::new();
        paste_events(&mut buf);
        let v_down = buf
            .iter()
            .find(|i| {
                let ki = unsafe { i.Anonymous.ki };
                ki.wVk == VK_V && ki.dwFlags.0 & KEYEVENTF_KEYUP.0 == 0
            })
            .expect("应有 V 的按下事件");
        assert_eq!(unsafe { v_down.Anonymous.ki.wScan }, scan_code(VK_V));
    }

    /// 顺序错了就不是粘贴：Ctrl 少按一次会打出字面的 v，
    /// Ctrl 少松一次会让主人之后敲的每个键都变成 Ctrl+键
    #[test]
    fn paste_is_ctrl_down_v_down_v_up_ctrl_up() {
        let mut buf = Vec::new();
        paste_events(&mut buf);
        let seq: Vec<(u16, bool)> = buf
            .iter()
            .map(|i| {
                let ki = unsafe { i.Anonymous.ki };
                (ki.wVk.0, ki.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0)
            })
            .collect();

        let ctrl_down = seq
            .iter()
            .position(|(vk, up)| *vk == VK_LCONTROL.0 && !up)
            .expect("缺 Ctrl 按下");
        let v_down = seq
            .iter()
            .position(|(vk, up)| *vk == VK_V.0 && !up)
            .expect("缺 V 按下");
        let v_up = seq
            .iter()
            .position(|(vk, up)| *vk == VK_V.0 && *up)
            .expect("缺 V 松开");
        let ctrl_up = seq
            .iter()
            .rposition(|(vk, up)| *vk == VK_LCONTROL.0 && *up)
            .expect("缺 Ctrl 松开");

        assert!(
            ctrl_down < v_down && v_down < v_up && v_up < ctrl_up,
            "顺序必须是 Ctrl↓ V↓ V↑ Ctrl↑，实际 {seq:?}"
        );
    }

    /// 批次末尾必须再清一次修饰键：目标程序吞掉某个 KEYUP 时 Ctrl 会卡住
    #[test]
    fn paste_ends_with_modifier_reset() {
        let mut buf = Vec::new();
        paste_events(&mut buf);
        let muts = crate::hotkey::MODIFIERS.len();
        let tail = &buf[buf.len() - muts..];
        assert!(
            tail.iter()
                .all(|i| unsafe { i.Anonymous.ki.dwFlags.0 } & KEYEVENTF_KEYUP.0 != 0),
            "批次末尾必须是全部修饰键的 KEYUP"
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
        push_key_stroke(&mut buf, VK_RETURN);
        assert_eq!(unsafe { buf[0].Anonymous.ki.wVk }, VK_RETURN);
        assert_eq!(
            unsafe { buf[0].Anonymous.ki.wScan },
            scan_code(VK_RETURN),
            "回车必须带真实扫描码（只读扫描码的程序才认，wScan=0 时它换不了行）"
        );
    }

    /// 回归（字打了、退格删不掉 / 回车不换行）：退格、回车、Tab 以前都传
    /// `wScan = 0`，而只读扫描码的程序（游戏、部分远程桌面）看不见这类事件。
    /// 它们必须和 `paste_events` 一样带真实扫描码。
    #[test]
    fn named_keys_carry_real_scan_codes() {
        for vk in [VK_BACK, VK_RETURN, VK_TAB] {
            assert_ne!(scan_code(vk), 0, "{vk:?} 的扫描码不该是 0");
            let mut buf = Vec::new();
            push_key_stroke(&mut buf, vk);
            assert_eq!(buf.len(), 2, "一次按键应该是「按下+松开」两个事件");
            for ev in &buf {
                let ki = unsafe { ev.Anonymous.ki };
                assert_eq!(ki.wVk, vk, "命名键的虚拟键码要保留");
                assert_ne!(ki.wScan, 0, "{vk:?} 的事件扫描码是 0，读扫描码的程序看不到");
            }
        }
    }

    /// `type_text` 的计数口径：UTF-16 单元数，但 `\r` 不计入
    /// （`\r` 不产生事件）。上层靠它和返回值比对，口径写错就会误判成"没打全"。
    #[test]
    fn typeable_units_counts_units_without_cr() {
        assert_eq!(typeable_units("你好"), 2);
        assert_eq!(typeable_units("a\r\nb"), 3, "\\r 不产生按键，不该计入");
        // 代理对（生僻字）编码成 2 个单元，按 2 算
        assert_eq!(typeable_units("\u{20BB7}"), 2);
    }

    /// 补发的 Win 按键必须是「按下 → 松开」，而且要带扩展键标记与我们的标记。
    /// 事件序列单独测：真调 `press_win` 会当场弹出开始菜单。
    #[test]
    fn win_press_replay_is_down_then_up_with_extended_flag() {
        let mut buf = Vec::new();
        win_press_events(&mut buf, 0x5B);
        assert_eq!(buf.len(), 2, "一次完整按键 = 按下 + 松开");
        for ev in &buf {
            let ki = unsafe { ev.Anonymous.ki };
            assert_eq!(ki.wVk.0, 0x5B);
            assert_ne!(
                ki.dwFlags.0 & KEYEVENTF_EXTENDEDKEY.0,
                0,
                "Win 是扩展键，少了这个标记外壳认不出来"
            );
            assert_eq!(ki.dwExtraInfo, INJECT_TAG, "缺标记会被自己的钩子当成真按键");
        }
        assert_eq!(
            unsafe { buf[0].Anonymous.ki.dwFlags.0 } & KEYEVENTF_KEYUP.0,
            0,
            "第一个事件必须是按下"
        );
        assert_ne!(
            unsafe { buf[1].Anonymous.ki.dwFlags.0 } & KEYEVENTF_KEYUP.0,
            0,
            "第二个事件必须是松开"
        );
    }

    /// Win+X 的补发顺序必须是「Win↓ 在前、当前键↓ 在后」，且在**同一个批次**里。
    /// 先放行当前键、再补 Win 的话，外壳收不到组合键（探针实测失效）。
    #[test]
    fn win_then_key_replay_puts_win_down_first() {
        let mut buf = Vec::new();
        win_then_key_events(&mut buf, 0x5B, 0x45, 0x12, false);
        assert_eq!(buf.len(), 2, "两条事件必须同批提交，不能分两次");
        let seq: Vec<(u16, u32, u16)> = buf
            .iter()
            .map(|i| {
                let ki = unsafe { i.Anonymous.ki };
                (ki.wVk.0, ki.dwFlags.0, ki.wScan)
            })
            .collect();
        assert_eq!(seq[0].0, 0x5B, "第一条必须是 Win 按下");
        assert_eq!(seq[0].1 & KEYEVENTF_KEYUP.0, 0, "Win 是按下");
        assert_eq!(seq[1].0, 0x45, "紧接着才是当前键");
        assert_eq!(seq[1].1 & KEYEVENTF_KEYUP.0, 0, "当前键也是按下");
        assert_eq!(seq[1].2, 0x12, "当前键的扫描码要原样带上（读扫描码的程序才认）");
        assert_ne!(seq[0].1 & KEYEVENTF_EXTENDEDKEY.0, 0, "Win 是扩展键");

        // 扩展键的另一侧：当前键带扩展位时也要照带上（如 Win+方向键）
        let mut buf2 = Vec::new();
        win_then_key_events(&mut buf2, 0x5B, 0x25, 0x4B, true);
        assert_ne!(
            unsafe { buf2[1].Anonymous.ki.dwFlags.0 } & KEYEVENTF_EXTENDEDKEY.0,
            0,
            "当前键是扩展键时要带上扩展位"
        );
    }
}
