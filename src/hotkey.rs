//! 全局快捷键：WH_KEYBOARD_LL 低级键盘钩子，独立线程跑消息循环。
//! 用钩子而不是 RegisterHotKey，因为按住说话需要「按下」和「松开」两个时刻。
//! 命中组合键时会吞掉事件（否则 Ctrl+1 会触发浏览器切标签）。

use anyhow::{bail, Result};
use eframe::egui;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::session::Cmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u32,
}

impl Default for Hotkey {
    /// 默认「Ctrl + Win」。`vk == 0` 表示这个组合里**没有主键**，
    /// 它完全由修饰键组成（见 `parse` 里对纯修饰键组合的说明）。
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            win: true,
            vk: 0,
        }
    }
}

impl Hotkey {
    /// 这个组合是否没有主键（全部由修饰键组成，如 Ctrl+Win）
    pub fn is_pure_modifiers(&self) -> bool {
        self.vk == 0
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        // 纯修饰键组合没有主键：`vk_name(0)` 会拼出一个不存在的 "VK00"，
        // 显示成「Ctrl + Win + VK00」，看着像坏了
        if !self.is_pure_modifiers() {
            parts.push(vk_name(self.vk));
        }
        parts.join(" + ")
    }

    /// 序列化成配置里的字符串，如 "ctrl+shift+f9"
    pub fn to_config(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl".to_string());
        }
        if self.alt {
            parts.push("alt".to_string());
        }
        if self.shift {
            parts.push("shift".to_string());
        }
        if self.win {
            parts.push("win".to_string());
        }
        // 同上：纯修饰键组合不能把那个占位的 0 写进配置，
        // 否则写出来是 "ctrl+win+vk00"，下次启动就解析不出来了
        if !self.is_pure_modifiers() {
            parts.push(vk_name(self.vk).to_lowercase());
        }
        parts.join("+")
    }
}

/// 主键总表（唯一事实来源）：egui 按键 → Windows 虚拟键码 → 名字。
///
/// 为什么只能有一份：录到的主键要走完「egui 按键 → 虚拟键码 → 配置字符串 →
/// 下次启动解析回来」四道关，任何一道对不上都不是报错，而是**看起来正常、
/// 实际失效**。之前这里有三个各自为政的映射，分号就是现成的例子：
/// 录得出来、却按 `VK{vk:02X}` 写成 `VKBA`，解析时直接报「不支持的按键」——
/// 主人只会看到"录了等于没录"。
///
/// 名字就是键帽上的字符（`;`、`[`、`F9`），界面显示直接用；写进配置时统一小写。
/// 同一个物理键会有多个 egui 按键（`;` 与 `:` 都在 0xBA 上），它们的键码和名字
/// 取同一份，所以不会出现"两个名字指同一个键"的歧义。
const MAIN_KEYS: &[(egui::Key, u32, &str)] = &[
    // 数字与字母
    (egui::Key::Num0, 0x30, "0"),
    (egui::Key::Num1, 0x31, "1"),
    (egui::Key::Num2, 0x32, "2"),
    (egui::Key::Num3, 0x33, "3"),
    (egui::Key::Num4, 0x34, "4"),
    (egui::Key::Num5, 0x35, "5"),
    (egui::Key::Num6, 0x36, "6"),
    (egui::Key::Num7, 0x37, "7"),
    (egui::Key::Num8, 0x38, "8"),
    (egui::Key::Num9, 0x39, "9"),
    (egui::Key::A, 0x41, "A"),
    (egui::Key::B, 0x42, "B"),
    (egui::Key::C, 0x43, "C"),
    (egui::Key::D, 0x44, "D"),
    (egui::Key::E, 0x45, "E"),
    (egui::Key::F, 0x46, "F"),
    (egui::Key::G, 0x47, "G"),
    (egui::Key::H, 0x48, "H"),
    (egui::Key::I, 0x49, "I"),
    (egui::Key::J, 0x4A, "J"),
    (egui::Key::K, 0x4B, "K"),
    (egui::Key::L, 0x4C, "L"),
    (egui::Key::M, 0x4D, "M"),
    (egui::Key::N, 0x4E, "N"),
    (egui::Key::O, 0x4F, "O"),
    (egui::Key::P, 0x50, "P"),
    (egui::Key::Q, 0x51, "Q"),
    (egui::Key::R, 0x52, "R"),
    (egui::Key::S, 0x53, "S"),
    (egui::Key::T, 0x54, "T"),
    (egui::Key::U, 0x55, "U"),
    (egui::Key::V, 0x56, "V"),
    (egui::Key::W, 0x57, "W"),
    (egui::Key::X, 0x58, "X"),
    (egui::Key::Y, 0x59, "Y"),
    (egui::Key::Z, 0x5A, "Z"),
    // 功能键（F1~F24 是 Windows 的虚拟键码上限，也是键盘上真实存在的范围）
    (egui::Key::F1, 0x70, "F1"),
    (egui::Key::F2, 0x71, "F2"),
    (egui::Key::F3, 0x72, "F3"),
    (egui::Key::F4, 0x73, "F4"),
    (egui::Key::F5, 0x74, "F5"),
    (egui::Key::F6, 0x75, "F6"),
    (egui::Key::F7, 0x76, "F7"),
    (egui::Key::F8, 0x77, "F8"),
    (egui::Key::F9, 0x78, "F9"),
    (egui::Key::F10, 0x79, "F10"),
    (egui::Key::F11, 0x7A, "F11"),
    (egui::Key::F12, 0x7B, "F12"),
    (egui::Key::F13, 0x7C, "F13"),
    (egui::Key::F14, 0x7D, "F14"),
    (egui::Key::F15, 0x7E, "F15"),
    (egui::Key::F16, 0x7F, "F16"),
    (egui::Key::F17, 0x80, "F17"),
    (egui::Key::F18, 0x81, "F18"),
    (egui::Key::F19, 0x82, "F19"),
    (egui::Key::F20, 0x83, "F20"),
    (egui::Key::F21, 0x84, "F21"),
    (egui::Key::F22, 0x85, "F22"),
    (egui::Key::F23, 0x86, "F23"),
    (egui::Key::F24, 0x87, "F24"),
    // 空格与编辑键
    (egui::Key::Space, 0x20, "Space"),
    (egui::Key::Tab, 0x09, "Tab"),
    (egui::Key::Enter, 0x0D, "Enter"),
    (egui::Key::Escape, 0x1B, "Esc"),
    (egui::Key::Backspace, 0x08, "Backspace"),
    (egui::Key::Insert, 0x2D, "Insert"),
    (egui::Key::Delete, 0x2E, "Delete"),
    (egui::Key::Home, 0x24, "Home"),
    (egui::Key::End, 0x23, "End"),
    (egui::Key::PageUp, 0x21, "PageUp"),
    (egui::Key::PageDown, 0x22, "PageDown"),
    // 方向键
    (egui::Key::ArrowLeft, 0x25, "Left"),
    (egui::Key::ArrowUp, 0x26, "Up"),
    (egui::Key::ArrowRight, 0x27, "Right"),
    (egui::Key::ArrowDown, 0x28, "Down"),
    // 符号键（同一个物理键的多个写法共用一行，如 Colon 与 Semicolon）
    (egui::Key::Backtick, 0xC0, "`"),
    (egui::Key::Minus, 0xBD, "-"),
    (egui::Key::Equals, 0xBB, "="),
    (egui::Key::Plus, 0xBB, "="),
    (egui::Key::OpenBracket, 0xDB, "["),
    (egui::Key::OpenCurlyBracket, 0xDB, "["),
    (egui::Key::CloseBracket, 0xDD, "]"),
    (egui::Key::CloseCurlyBracket, 0xDD, "]"),
    (egui::Key::Backslash, 0xDC, "\\"),
    (egui::Key::Pipe, 0xDC, "\\"),
    (egui::Key::Semicolon, 0xBA, ";"),
    (egui::Key::Colon, 0xBA, ";"),
    (egui::Key::Quote, 0xDE, "'"),
    (egui::Key::Comma, 0xBC, ","),
    (egui::Key::Period, 0xBE, "."),
    (egui::Key::Slash, 0xBF, "/"),
    (egui::Key::Questionmark, 0xBF, "/"),
    (egui::Key::Exclamationmark, 0x31, "1"),
    (egui::Key::IntlBackslash, 0xE2, "Oem102"),
];

/// 手写配置里可能出现的别名（老版本用过或口语写法）。
/// 只做"读得懂"，不作为显示名 —— 显示名只认 `MAIN_KEYS`，否则同一份配置
/// 会出现两种写法，比对时就分不清"主人改过"和"我们写的"。
const KEY_ALIASES: &[(&str, u32)] = &[
    ("return", 0x0D),
    ("escape", 0x1B),
    ("backtick", 0xC0),
    ("grave", 0xC0),
    ("oem_102", 0xE2),
];

pub fn vk_name(vk: u32) -> String {
    MAIN_KEYS
        .iter()
        .find(|(_, v, _)| *v == vk)
        .map(|(_, _, n)| (*n).to_string())
        .unwrap_or_else(|| format!("VK{vk:02X}"))
}

/// egui 按键 → 虚拟键码。录到的键必须能在 [`MAIN_KEYS`] 里找到，
/// 否则一律不认（宁可让主人换一个键，也不能存下一个解析不回来的字符串）。
pub fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    MAIN_KEYS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, v, _)| *v)
}

/// 这个 egui 按键是不是修饰键本身。
///
/// 为什么必须在捕捉时把它挑出来丢掉：egui 0.35 **会**为左右 Ctrl / Win
/// 产生 `Event::Key`（见 egui-winit 的 `KeyCode::ControlLeft => Key::ControlLeft`），
/// 但修饰键不是"主键" —— 它们由钩子统一记录（见 [`current_mods`]）。
/// 以前这类事件会被当成"不支持的按键"，于是主人**刚按下 Ctrl 的瞬间**捕捉就报错
/// 并清空半截状态，Ctrl+Win 永远录不出来。
pub fn is_modifier_key(key: egui::Key) -> bool {
    use egui::Key;
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// 解析 "ctrl+1" / "alt+space" / "f9" / "ctrl+shift+f9" / "ctrl+win" / "ctrl+alt+shift+win"
///
/// 组合方式**不设白名单**：Ctrl / Alt / Shift / Win 随意搭配（1~4 个都行），
/// 主键覆盖 [`MAIN_KEYS`] 里的全部按键（字母、数字、F1~F24、方向键、翻页键、
/// 空格/回车/退格，以及 `;` `[` `-` 这类符号键）。只有两条是硬性安全线：
///
/// 1. **单独的普通键不行**（如只写 `a`）：那会把这个键在**所有程序里**都吞掉，
///    主人以后就再也打不出这个字母了；单独的功能键（`f9`）可以，因为 F 键
///    平时不参与打字。
/// 2. **单独一个修饰键也不行**（如只写 `ctrl`）：系统层面区分不出"主人按住
///    Ctrl 说话"和"主人要用 Ctrl+C"，放开会连带把所有 Ctrl 组合键弄坏。
pub fn parse(text: &str) -> Result<Hotkey> {
    let mut hk = Hotkey {
        ctrl: false,
        alt: false,
        shift: false,
        win: false,
        vk: 0,
    };
    let mut key: Option<u32> = None;
    for raw in text.split('+') {
        let t = raw.trim().to_lowercase();
        if t.is_empty() {
            continue;
        }
        match t.as_str() {
            "ctrl" | "control" => hk.ctrl = true,
            "alt" => hk.alt = true,
            "shift" => hk.shift = true,
            "win" | "super" | "meta" => hk.win = true,
            _ => {
                let vk = key_vk(&t)?;
                if key.replace(vk).is_some() {
                    bail!("只能指定一个主键：{text}");
                }
            }
        }
    }
    let mods = [hk.ctrl, hk.alt, hk.shift, hk.win]
        .iter()
        .filter(|on| **on)
        .count();
    match key {
        Some(vk) => {
            hk.vk = vk;
            if mods == 0 && !is_function_key(vk) {
                bail!(
                    "按键「{}」要搭配至少一个修饰键（Ctrl/Alt/Shift/Win）；只有 F1~F24 能单独当快捷键",
                    vk_name(vk)
                );
            }
        }
        None => {
            if mods < 2 {
                bail!("纯修饰键组合至少要两个修饰键，例如 Ctrl+Win");
            }
        }
    }
    Ok(hk)
}

fn key_vk(t: &str) -> Result<u32> {
    if let Some((_, vk, _)) = MAIN_KEYS.iter().find(|(_, _, n)| n.eq_ignore_ascii_case(t)) {
        return Ok(*vk);
    }
    if let Some((_, vk)) = KEY_ALIASES.iter().find(|(n, _)| n.eq_ignore_ascii_case(t)) {
        return Ok(*vk);
    }
    bail!("不支持的按键：{t}")
}

/// 是不是功能键（F1~F24）。单独一个键当快捷键只允许功能键，原因见 [`parse`]。
fn is_function_key(vk: u32) -> bool {
    (0x70..=0x87).contains(&vk)
}

struct HookCtx {
    tx: UnboundedSender<Cmd>,
    paused: Arc<AtomicBool>,
    ctrl: AtomicBool,
    alt: AtomicBool,
    shift: AtomicBool,
    win: AtomicBool,
    armed: AtomicBool,
    /// 触发键的 keydown 被我们吞过（决定它的 keyup 是否也吞）。
    /// 存的是**虚拟键码**（0 = 没有）：纯修饰键组合（Ctrl+Win）里被吞的是
    /// 某个修饰键，必须记住具体是哪一个 —— 否则先松开 Ctrl 结束录音后，
    /// 还欠着 Win 的 keyup 就没人吞了，开始菜单会莫名其妙弹出来。
    held_vk: AtomicU32,
    /// 被我们**临时**接管的 Win 键（0 = 没有）。
    ///
    /// 为什么 Win 的按下要一律吞掉：组合里带着 Win 时，只要那次按下原样放给外壳，
    /// 外壳就记下了"Win 按下过"，之后谁也擦不掉 —— 松开时它必定弹开始菜单抢走焦点，
    /// 正在说话的主人这一次结果就打不进目标程序（主人报的"突然断开、然后像只按着
    /// Win 那样弹出开始界面"就是这个）。反过来"吞掉 Win 的松开"也不行：外壳会一直
    /// 以为 Win 按着，之后主人随便敲一个字母都会触发 Win+X（打开资源管理器）。
    /// 两条路都用注入探针实测过，所以只剩"按下就接管、事后按主人的真实意图补发"。
    pending_win: AtomicU32,
    /// 已被收编进本次「按住说话」的 Win 键（0 = 没有）：组合凑齐了，
    /// 它就是这次快捷键的一部分，松开照吞、绝不补发（补发就等于弹开始菜单）。
    session_win: AtomicU32,
}

static CTX: OnceLock<HookCtx> = OnceLock::new();

/// 补发 Win 键失败的次数与最近一次的错误码。
///
/// 为什么只记数字、不在钩子里写日志：低级键盘钩子**必须毫秒级返回**，而 `log()` 要
/// 抢锁、写磁盘 —— 一旦被卡住（杀软扫描、磁盘忙），Windows 会**静默摘掉这个钩子**，
/// 表现就是"快捷键突然全失效、Win 键开始乱弹"，日志里还什么都看不到。
/// 所以钩子回调里只准动原子变量，真正的日志由会话线程在开新会话时补记
/// （见 [`take_replay_failures`]）。
static REPLAY_FAILURES: AtomicU32 = AtomicU32::new(0);
static REPLAY_LAST_ERROR: AtomicU32 = AtomicU32::new(0);

/// 取走"上一次补发失败"的计数与错误码（取走即清零）。
/// 由会话线程调用并写进日志 —— 见 [`REPLAY_FAILURES`] 的说明。
pub fn take_replay_failures() -> (u32, u32) {
    (
        REPLAY_FAILURES.swap(0, Ordering::Relaxed),
        REPLAY_LAST_ERROR.swap(0, Ordering::Relaxed),
    )
}

/// 记一次补发失败。**只能在钩子回调里调**，所以只动原子变量。
fn note_replay_failure() {
    REPLAY_FAILURES.fetch_add(1, Ordering::Relaxed);
    // 错误码取"最近一次 Win32 错误"：`injector` 的报错里其实也带着它，
    // 但从 `anyhow::Error` 里拆字符串再解析太吵，这里重读一次更直接（读不到就是 0）。
    let code = unsafe { windows::Win32::Foundation::GetLastError() }.0;
    REPLAY_LAST_ERROR.store(code, Ordering::Relaxed);
}

// 当前生效的组合键，改动后立即生效（不必重启）。
// 初值与 `Hotkey::default()`（Ctrl+Win，无主键）保持一致。
static CURRENT_VK: AtomicU32 = AtomicU32::new(0);
static CURRENT_MODS: AtomicU32 = AtomicU32::new(9); // bit0 Ctrl / bit1 Alt / bit2 Shift / bit3 Win

pub fn set_current(hk: Hotkey) {
    CURRENT_VK.store(hk.vk, Ordering::Relaxed);
    CURRENT_MODS.store(mods_bits(&hk), Ordering::Relaxed);
    // 换键时必须清掉「按住中」状态：否则旧键的 keyup 匹配不上新键，
    // armed 会一直挂着 —— 会话收不到 Stop，录音停不下来。
    // 两个 Win 记账字段同理：留着它们会在下次按别的键时补发一个早已过期的
    // Win 事件（凭空弹一次开始菜单，或者把 Win+X 送到别的程序里）。
    if let Some(ctx) = CTX.get() {
        ctx.armed.store(false, Ordering::Relaxed);
        ctx.held_vk.store(0, Ordering::Relaxed);
        ctx.pending_win.store(0, Ordering::Relaxed);
        ctx.session_win.store(0, Ordering::Relaxed);
    }
}

pub fn current() -> Hotkey {
    let (ctrl, alt, shift, win) = unpack_mods(CURRENT_MODS.load(Ordering::Relaxed));
    Hotkey {
        ctrl,
        alt,
        shift,
        win,
        vk: CURRENT_VK.load(Ordering::Relaxed),
    }
}

/// 此刻真正按住的修饰键（含 Win）。
///
/// 设置界面的改键捕捉必须用它、而不是 egui 的 `Modifiers`：
/// egui 的 `Modifiers` **没有 Win 字段**，egui 也不为纯修饰键（Ctrl/Win 自身）
/// 产生按键事件 —— 靠 egui 事件录不出 Ctrl+Win 这个组合。
/// 钩子在 `paused`（捕捉）期间照样维护修饰键状态，所以这里读得到。
pub fn current_mods() -> Mods {
    CTX.get().map(read_mods).unwrap_or_default()
}

/// 修饰键位打包（`CURRENT_MODS` 的编码）。与 [`unpack_mods`] 成对 ——
/// 单独抽出来是为了让"打包→解包必须还原"这件事**可以在不碰静态变量的前提下**
/// 被单测覆盖（见 mod tests 顶部关于进程级静态变量的说明）。
fn mods_bits(hk: &Hotkey) -> u32 {
    (hk.ctrl as u32) | ((hk.alt as u32) << 1) | ((hk.shift as u32) << 2) | ((hk.win as u32) << 3)
}

/// 位解包，返回 (ctrl, alt, shift, win)
fn unpack_mods(m: u32) -> (bool, bool, bool, bool) {
    (m & 1 != 0, m & 2 != 0, m & 4 != 0, m & 8 != 0)
}

/// 解析配置里的快捷键并立即让正在运行的钩子生效（不必重启）
pub fn apply_from_config(text: &str) -> Result<Hotkey> {
    let hk = parse(text)?;
    set_current(hk);
    crate::log::log(format!("快捷键已生效：{}", hk.display()));
    Ok(hk)
}

/// 启动钩子线程。`paused` 为 true 时钩子完全不起作用（供设置界面捕捉快捷键用）。
pub fn spawn(hk: Hotkey, tx: UnboundedSender<Cmd>, paused: Arc<AtomicBool>) -> Result<()> {
    set_current(hk);
    crate::log::log(format!("注册全局快捷键：{}", hk.display()));
    CTX.set(HookCtx {
        tx,
        paused,
        ctrl: AtomicBool::new(false),
        alt: AtomicBool::new(false),
        shift: AtomicBool::new(false),
        win: AtomicBool::new(false),
        armed: AtomicBool::new(false),
        held_vk: AtomicU32::new(0),
        pending_win: AtomicU32::new(0),
        session_win: AtomicU32::new(0),
    })
    .ok();

    std::thread::Builder::new()
        .name("bbvoxi-hotkey".into())
        .spawn(|| unsafe {
            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                Ok(h) => h,
                Err(e) => {
                    crate::log::log(format!("安装全局键盘钩子失败：{e}"));
                    return;
                }
            };
            let mut msg = MSG::default();
            // `GetMessageW` 的返回值：>0 正常、0 是 WM_QUIT、**-1 是出错**。
            // 以前写的是 `.as_bool()`（非 0 即真），于是 -1 被当成"还有消息" ——
            // 空转刷 CPU，而钩子其实已经不工作了，还一声不响。
            loop {
                let result = GetMessageW(&mut msg, None, 0, 0);
                if result.0 <= 0 {
                    if result.0 < 0 {
                        // 这里不在钩子回调里（是钩子线程自己的循环），写日志没问题
                        crate::log::log("钩子线程消息循环出错（GetMessageW 返回 -1），已退出");
                    }
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(hook);
        })?;
    Ok(())
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(ctx) = CTX.get() else {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    const LLKHF_INJECTED: u32 = 0x10;
    const LLKHF_EXTENDED: u32 = 0x01;
    let injected = kb.flags.0 & LLKHF_INJECTED != 0;
    // 双保险：除了系统标记，还看我们自己的 dwExtraInfo 标记
    let ours = kb.dwExtraInfo == crate::injector::INJECT_TAG;
    let vk = kb.vkCode;
    let msg = wparam.0 as u32;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;

    // 注入事件必须完全忽略：我们自己在打字前会注入修饰键 KEYUP（把 Ctrl「清掉」），
    // 如果让它参与状态跟踪，钩子会误判成"用户松开了 Ctrl"，
    // 于是触发键不再被吞 → 前台程序收到 1 的自动重复，出现满屏 111111。
    if injected || ours {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // 修饰键状态跟着事件走（左右变体由 classify_modifier 统一处理）。
    // 设置界面捕捉快捷键期间也要更新：否则捕捉结束时用户还按着 Ctrl，
    // 这里的状态却是旧值，接下来按组合键会失灵（得先松手再按一次才恢复）。
    write_mods(ctx, apply_mods(read_mods(ctx), vk, down));

    // 捕捉快捷键期间只维护状态，不触发也不吞键（按键要原样交给界面）。
    // **Win 键是唯一的例外**：它一按下（或松开）就会弹出开始菜单并抢走焦点，
    // 而捕捉逻辑一旦发现窗口失焦就立刻结束 —— 于是"按下 Win 的瞬间捕捉就没了"，
    // Ctrl+Win 永远录不上。所以捕捉期间必须把 Win 彻底吞掉：这样一来外壳什么
    // 都没看见，既不会弹开始菜单、也不会留下"Win 还按着"的假状态。
    // 顺带把「临时接管」的记账清掉：那次按下的 keyup 正好在这里被吞，
    // 留着的记账会在捕捉结束后拿着一次早已过期的 Win 去补发。
    if ctx.paused.load(Ordering::Relaxed) {
        if classify_modifier(vk) == Some(ModKind::Win) {
            ctx.pending_win.store(0, Ordering::Relaxed);
            ctx.session_win.store(0, Ordering::Relaxed);
            return LRESULT(1);
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    {
        let mods = read_mods(ctx);
        let hold = Hold {
            armed: ctx.armed.load(Ordering::Relaxed),
            held_vk: ctx.held_vk.load(Ordering::Relaxed),
            pending_win: ctx.pending_win.load(Ordering::Relaxed),
            session_win: ctx.session_win.load(Ordering::Relaxed),
        };
        let decision = decide(current(), mods, vk, down, hold);
        ctx.armed.store(decision.armed, Ordering::Relaxed);
        ctx.held_vk.store(decision.held_vk, Ordering::Relaxed);
        ctx.pending_win.store(decision.pending_win, Ordering::Relaxed);
        ctx.session_win.store(decision.session_win, Ordering::Relaxed);

        // 还账：Win 的按下被我们接管了，主人真正想干的那件事在这里补给他（见 `Replay`）。
        // 补发放在状态写回之后，而且必须在返回之前 —— 我们注入的事件会被这个钩子
        // 立刻重新收到（注入的一律原样放行），状态先落定，重入的那一次才只是"看一眼"。
        // 这里**只记数字不写日志**（钩子回调里禁止 I/O，理由见 `REPLAY_FAILURES`）。
        match decision.replay {
            Replay::None => {}
            Replay::WinPress(win) => {
                if crate::injector::press_win(win).is_err() {
                    note_replay_failure();
                }
            }
            Replay::WinThenCurrentKey(win) => {
                let extended = kb.flags.0 & LLKHF_EXTENDED != 0;
                let scan = kb.scanCode as u16;
                if crate::injector::win_then_key(win, vk, scan, extended).is_err() {
                    note_replay_failure();
                }
            }
        }

        match decision.trigger {
            Some(Trigger::Start) => {
                // 这里**不写日志**：低级键盘钩子必须在毫秒级返回，文件 I/O
                // 一旦卡住（杀软、磁盘忙），Windows 会悄悄摘掉这个钩子 ——
                // 表现就是"按键没反应、Win 键突然开始弹开始菜单"，且毫无提示。
                // 日志由会话线程在收到指令时补记（见 session::worker）。
                let _ = ctx.tx.send(Cmd::Start);
            }
            Some(Trigger::Stop) => {
                let _ = ctx.tx.send(Cmd::Stop);
            }
            None => {}
        }
        if decision.swallow {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 读出当前按住的修饰键
fn read_mods(ctx: &HookCtx) -> Mods {
    Mods {
        ctrl: ctx.ctrl.load(Ordering::Relaxed),
        alt: ctx.alt.load(Ordering::Relaxed),
        shift: ctx.shift.load(Ordering::Relaxed),
        win: ctx.win.load(Ordering::Relaxed),
    }
}

fn write_mods(ctx: &HookCtx, mods: Mods) {
    ctx.ctrl.store(mods.ctrl, Ordering::Relaxed);
    ctx.alt.store(mods.alt, Ordering::Relaxed);
    ctx.shift.store(mods.shift, Ordering::Relaxed);
    ctx.win.store(mods.win, Ordering::Relaxed);
}

/// 按一次按键事件更新修饰键状态（纯函数，便于单测）
pub fn apply_mods(mods: Mods, vk: u32, down: bool) -> Mods {
    match classify_modifier(vk) {
        Some(ModKind::Ctrl) => Mods { ctrl: down, ..mods },
        Some(ModKind::Alt) => Mods { alt: down, ..mods },
        Some(ModKind::Shift) => Mods {
            shift: down,
            ..mods
        },
        Some(ModKind::Win) => Mods { win: down, ..mods },
        None => mods,
    }
}

/// 修饰键分类 —— 单一事实来源。
///
/// 关键：Windows 低级键盘钩子上报的是**左右分开**的虚拟键码
/// （左 Ctrl = VK_LCONTROL 0xA2、右 Ctrl = VK_RCONTROL 0xA3 …），
/// 它**不会**上报通用的 VK_CONTROL(0x11)。之前就是漏了 0xA2，
/// 导致物理左 Ctrl 从未被记入状态，Ctrl+X 类快捷键全都失效。
pub fn classify_modifier(vk: u32) -> Option<ModKind> {
    MODIFIERS
        .iter()
        .find(|(key, _, _)| key.0 as u32 == vk)
        .map(|(_, kind, _)| *kind)
}

/// 修饰键表（唯一来源）：(虚拟键码, 归属, 是否扩展键)
/// 注入模块也用这张表生成"修饰键重置"，避免两处各写一份再走偏。
pub const MODIFIERS: [(VIRTUAL_KEY, ModKind, bool); 11] = [
    (VK_CONTROL, ModKind::Ctrl, false),
    (VK_LCONTROL, ModKind::Ctrl, false),
    (VK_RCONTROL, ModKind::Ctrl, true),
    (VK_MENU, ModKind::Alt, false),
    (VK_LMENU, ModKind::Alt, false),
    (VK_RMENU, ModKind::Alt, true),
    (VK_SHIFT, ModKind::Shift, false),
    (VK_LSHIFT, ModKind::Shift, false),
    (VK_RSHIFT, ModKind::Shift, true),
    (VK_LWIN, ModKind::Win, true),
    (VK_RWIN, ModKind::Win, true),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKind {
    Ctrl,
    Alt,
    Shift,
    Win,
}

fn is_modifier(vk: u32) -> bool {
    classify_modifier(vk).is_some()
}

/// 这个键是不是"当前组合键**要求**按住的修饰键"。
///
/// 与 `is_modifier` 的区别：组合是 Ctrl+1 时按住 Ctrl+Shift+1 再松开 Shift，
/// Shift 虽然是修饰键、却不在组合里，松开它不该结束录音。
fn required_modifier(hk: Hotkey, vk: u32) -> bool {
    match classify_modifier(vk) {
        Some(ModKind::Ctrl) => hk.ctrl,
        Some(ModKind::Alt) => hk.alt,
        Some(ModKind::Shift) => hk.shift,
        Some(ModKind::Win) => hk.win,
        None => false,
    }
}

/// 当前按住的修饰键
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Start,
    Stop,
}

/// 钩子记的「按住说话」状态。
///
/// 抽成一个纯数据、当参数传进 [`decide`]：钩子回调本身没法单测，
/// 而"接管 Win 键"这套记账的正确性全在这里 —— 装进进程级静态变量就测不了
/// （并行跑的单例会互相污染），所以状态一律显式传递。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hold {
    /// 正处于「按住说话」
    pub armed: bool,
    /// 触发键的 keydown 被我们吞过（决定它的 keyup 是否也吞）。0 = 没有
    pub held_vk: u32,
    /// 被我们临时接管的 Win 键（0 = 没有）：还欠一次补发，
    /// 因为主人可能只是想按 Win 打开开始菜单，也可能正在按 Win+X
    pub pending_win: u32,
    /// 已被收编进本次「按住说话」的 Win 键（0 = 没有）：松开照吞、不补发
    pub session_win: u32,
}

/// 需要钩子替主人补发的事件。
///
/// 起因：组合里带 Win 时，Win 的按下**必须由我们接管**（见 [`Hold::pending_win`]），
/// 接管之后就欠系统一次"真正的 Win 按键"，得在搞清楚主人想干什么时还回去：
/// - 只按了 Win（想打开开始菜单）→ 补一整套按下+松开；
/// - 按了 Win+X（想用系统组合键）→ 补 Win 按下 + 当前键的按下。
///
/// 两种补法都是拿注入探针在真机上实测出来的：补一整套 Win 按下+松开，开始菜单
/// 会正常弹出；而"Win 按下 + 当前键"必须**同一次 `SendInput` 提交** ——
/// 分两次提交、或者先把当前键放行再补 Win，外壳都会先收到那个键，组合键不生效
/// （探针 C2 就是这么失效的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Replay {
    /// 什么都不用补
    #[default]
    None,
    /// 补一整套 Win 按下+松开（参数 = 要补的 Win 键码）
    WinPress(u32),
    /// 一次提交「Win 按下 + 当前键按下」（参数 = 要补的 Win 键码）
    WinThenCurrentKey(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub trigger: Option<Trigger>,
    /// 是否吞掉这个按键（命中组合键时吞掉，避免 Ctrl+1 触发浏览器切标签，
    /// 也避免 Ctrl+Win 弹出开始菜单）
    pub swallow: bool,
    /// 处理之后是否处于「按住中」状态
    pub armed: bool,
    /// 处理之后触发键是否仍处于「我们吞过它的 keydown」状态（0 = 没有）
    pub held_vk: u32,
    /// 处理之后临时接管的 Win 键（0 = 没有，见 [`Hold::pending_win`]）
    pub pending_win: u32,
    /// 处理之后收编进本次按住的 Win 键（0 = 没有，见 [`Hold::session_win`]）
    pub session_win: u32,
    /// 需要补发的事件（不补就等于把主人的 Win 按键吃掉了）
    pub replay: Replay,
}

/// 状态原样保留、不触发、不补发的决策（绝大多数按键走这条）
fn keep(hold: Hold, swallow: bool, replay: Replay) -> Decision {
    after_keyup(hold, swallow, None, replay)
}

/// 组装一次 keyup 决策：`after` = 结算过"谁的 keyup 该吞"之后的状态。
///
/// 抽出来是为了别让每个 keyup 分支都重列七个字段 —— 那种写法一旦加一个状态字段，
/// 四个分支都要改，漏一个就是一个静默的行为差异。
fn after_keyup(after: Hold, swallow: bool, trigger: Option<Trigger>, replay: Replay) -> Decision {
    Decision {
        trigger,
        swallow,
        armed: after.armed,
        held_vk: after.held_vk,
        pending_win: after.pending_win,
        session_win: after.session_win,
        replay,
    }
}

/// 判定逻辑抽成纯函数：钩子回调本身没法单元测试，
/// 而「按住说话」的正确性全靠这里（这块之前没人看着，就出过问题）。
///
/// `hold` = 处理之前的状态（见 [`Hold`]）。返回值除了"触不触发、吞不吞"，
/// 还带上处理之后的状态和**要补发给系统的事件**（[`Replay`]）。
///
/// 两条基础规则没变：
/// - keyup 只吞自己吞过 keydown 的那几个键 —— 裸按触发键（没按修饰键）时 keydown
///   已经放行给前台程序了，keyup 也必须放行，否则在游戏等跟踪 keyup 的程序里
///   表现为按键卡住；
/// - `hk.vk == 0` 是**纯修饰键组合**（如默认的 Ctrl+Win）：凑齐全部修饰键的那次
///   按下开始录音、松开**组合里要求**的任一修饰键结束录音。
///
/// 新增的是 Win 键的接管（组合里含 Win 时）。为什么非得接管，而不是"凑不齐就放行"：
/// 只要那次 Win 按下被放行，外壳就记住了"Win 按下过"，之后**没有任何办法**让它忘掉 ——
///   - 松开时让它过去 → 开始菜单弹出、抢走焦点，主人这次识别结果一个字都打不进
///     目标程序（他报的就是"突然断开，然后像只按着 Win 那样弹出开始界面"）；
///   - 把松开吞掉 → 外壳以为 Win 一直按着，之后主人随便敲个字母都会触发 Win+X
///     （打开资源管理器），比弹一下开始菜单更糟。
///
/// 两条都用注入探针在真机上实测确认过，所以只剩一条路：**按下就接管，事后按主人
/// 真正的意图补发**（[`Replay`]）——顺序无关，先 Ctrl 后 Win、先 Win 后 Ctrl 都对。
pub fn decide(hk: Hotkey, mods: Mods, vk: u32, down: bool, hold: Hold) -> Decision {
    let mods_ok =
        hk.ctrl == mods.ctrl && hk.alt == mods.alt && hk.shift == mods.shift && hk.win == mods.win;
    // 只需要接管"组合里确实要用 Win"的那种情况；组合里没有 Win 就不碰它
    let owns_win = hk.win;
    let is_win = classify_modifier(vk) == Some(ModKind::Win);

    if down {
        let hit = if hk.is_pure_modifiers() {
            is_modifier(vk) && mods_ok
        } else {
            vk == hk.vk && mods_ok
        };
        if hit {
            // 组合凑齐了：临时接管的那次 Win 就此**收编**进本次按住 ——
            // 它现在是快捷键的一部分，松开时照吞，绝不补发（补发=弹开始菜单）
            let session_win = if hold.pending_win != 0 {
                hold.pending_win
            } else {
                hold.session_win
            };
            return Decision {
                // 长按时系统会重复发 keydown，不能重复触发
                trigger: if hold.armed { None } else { Some(Trigger::Start) },
                swallow: true,
                armed: true,
                held_vk: vk,
                pending_win: 0,
                session_win,
                replay: Replay::None,
            };
        }
        // Win 的按下、且组合里要用 Win：一律接管（原因见上面的长注释）。
        // 已经开始录音之后过来的 Win 事件只可能是长按的自动重复 —— 那时它早就被
        // 收编进本次按住了，继续吞、记账一个字都不改（这里绝不能再记成"欠一次补发"，
        // 否则录音途中的自动重复会攒下一笔账，随后被随手一个字母触发成 Win+X）。
        if owns_win && is_win {
            if hold.armed || hold.session_win == vk || hold.held_vk == vk {
                return keep(hold, true, Replay::None);
            }
            // 第一次按下：先记下来，暂不补发（主人可能正要按 Ctrl）
            return keep(
                Hold {
                    pending_win: if hold.pending_win == 0 {
                        vk
                    } else {
                        hold.pending_win
                    },
                    ..hold
                },
                true,
                Replay::None,
            );
        }
        // 还欠着一次 Win 按下的时候，主人按了别的键（非修饰键）：他要用的是
        // Win+X 这类系统组合键 —— 把这次按下整套补给他（Win 与当前键必须一次提交）。
        // 两个附加条件都不是多余的：
        // - `mods.win`：那次 Win 必须**现在还按着**。这挡的是"账被打扫成半截"的情况
        //   （换键、捕捉结束时刚好撞上按键事件）—— 过期的账绝不能补出系统组合键；
        // - `!hold.armed`：录音途中绝不补发组合键，宁可这一次按键没生效。
        if hold.pending_win != 0 && mods.win && !hold.armed && !is_modifier(vk) {
            return keep(
                Hold {
                    pending_win: 0,
                    ..hold
                },
                true,
                Replay::WinThenCurrentKey(hold.pending_win),
            );
        }
        return keep(hold, false, Replay::None);
    }

    // —— keyup ——
    // 只吞自己吞过 keydown 的那几个键：触发键、以及被收编进本次按住的 Win
    // （`vk != 0` 是给注入事件留的保险：KEYEVENTF_UNICODE 的 vkCode 就是 0，
    // 别把"没有键"当成"就是这个键"）
    let mut after = hold;
    let swallow = vk != 0 && (hold.held_vk == vk || hold.session_win == vk);
    if vk != 0 && hold.held_vk == vk {
        after.held_vk = 0;
    }
    if vk != 0 && hold.session_win == vk {
        after.session_win = 0;
    }

    // 单独一次 Win 的按下被我们接管了，现在主人松开了它 —— 他就是想打开开始菜单
    // （组合一直没凑齐，否则这次 Win 早就被收编了）。吞掉原始松开、补一整套
    // Win 按下+松开，开始菜单照常弹出，而且系统里 Win 的状态自此是干净的。
    if owns_win && is_win && vk != 0 && hold.pending_win == vk {
        after.pending_win = 0;
        return after_keyup(after, true, None, Replay::WinPress(vk));
    }

    // 松开这个组合的"主键"就结束录音。纯修饰键组合没有主键，
    // 松掉组合里要求的任一修饰键就等于松开了它
    let released_main = if hk.is_pure_modifiers() {
        required_modifier(hk, vk)
    } else {
        vk == hk.vk
    };
    if released_main {
        after.armed = false;
        return after_keyup(
            after,
            swallow,
            hold.armed.then_some(Trigger::Stop),
            Replay::None,
        );
    }
    // 先松开修饰键（例如先放 Ctrl 再放 1）也要结束录音
    if hold.armed && required_modifier(hk, vk) {
        after.armed = false;
        return after_keyup(after, swallow, Some(Trigger::Stop), Replay::None);
    }
    after_keyup(after, swallow, None, Replay::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_combos() {
        let hk = parse("ctrl+1").unwrap();
        assert!(hk.ctrl && !hk.alt && hk.vk == 0x31);
        assert_eq!(hk.display(), "Ctrl + 1");
        assert_eq!(hk.to_config(), "ctrl+1");

        let hk = parse("Ctrl+Shift+F9").unwrap();
        assert!(hk.ctrl && hk.shift && hk.vk == 0x78);
        assert_eq!(hk.to_config(), "ctrl+shift+f9");

        assert!(parse("alt+space").unwrap().alt);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("1").is_err()); // 无修饰键的普通键
        assert!(parse("ctrl").is_err()); // 只有一个修饰键、又没有主键
        assert!(parse("ctrl+a+b").is_err()); // 多个主键
        assert!(parse("ctrl+f99").is_err());
        assert!(parse("f9").is_ok()); // 功能键可单独用
        assert!(parse("ctrl+win").is_ok()); // 纯修饰键组合（默认值）
        assert!(parse("win").is_err()); // 单个 Win 不许当快捷键（会弄坏开始菜单）
    }

    /// 默认值必须能解析回它自己 —— 默认组合是纯修饰键（没有主键），
    /// 序列化少写一块或解析少认一步，都会让"默认值"变成开机就报错的配置
    #[test]
    fn default_hotkey_roundtrips() {
        let d = Hotkey::default();
        assert_eq!(d.display(), "Ctrl + Win");
        assert_eq!(d.to_config(), "ctrl+win");
        let back = parse(&d.to_config()).expect("默认快捷键必须能被解析回来");
        assert_eq!(back, d);
    }

    /// 用户实际用的组合：Ctrl + 反引号（曾经因为"保存后没同步给钩子"而失效）
    ///
    /// 这里**刻意不调 `apply_from_config` / 不断言 `current()`**：
    /// 那两个都读写进程级静态变量 `CURRENT_VK`/`CURRENT_MODS`，单测并行跑时
    /// 会污染其他用例（这个坑踩过一次）。要验证"应用后钩子拿到的就是刚设置的
    /// 组合"，覆盖点拆成两半 —— 解析这半在下面用纯函数测，位打包那半由
    /// `mods_bits_round_trips_through_unpack` 覆盖；`apply_from_config` 本身
    /// 只剩 parse + set_current 两行接线。
    #[test]
    fn parses_user_hotkey_and_serializes_it_back() {
        let hk = parse("ctrl+`").unwrap();
        assert_eq!(hk.vk, 0xC0);
        assert!(hk.ctrl);
        assert_eq!(hk.to_config(), "ctrl+`");
    }

    /// 主键总表必须自洽：录（egui 按键 → 键码）、写（键码 → 名字）、
    /// 读（名字 → 键码）三道关要能往返走通。
    ///
    /// 这条守的是"录了等于没录"这类事故：分号以前录得出来，却被写成 `VKBA`，
    /// 保存时解析报「不支持的按键」，主人只会看到快捷键莫名其妙没生效。
    /// 表里任何一行写错（键码、名字对不上），这里都会当场炸出来。
    #[test]
    fn main_key_table_round_trips() {
        for &(key, vk, name) in MAIN_KEYS {
            assert_eq!(egui_key_to_vk(key), Some(vk), "{name} 的录键映射对不上");
            assert_eq!(vk_name(vk), name, "0x{vk:02X} 的名字对不上");
            assert_eq!(key_vk(name).unwrap(), vk, "{name} 解析不回来");
            assert_eq!(
                key_vk(&name.to_lowercase()).unwrap(),
                vk,
                "配置里存的是小写，必须也认"
            );
            let hk = parse(&format!("ctrl+{}", name.to_lowercase())).expect("每个主键都要能用");
            assert_eq!(hk.vk, vk, "{name} 解析出来的键码不对");
        }
    }

    /// 组合方式不设白名单：三个、四个修饰键的组合也要能用
    #[test]
    fn parses_three_and_four_modifier_combos() {
        let all = parse("ctrl+alt+shift+win").unwrap();
        assert!(all.ctrl && all.alt && all.shift && all.win && all.is_pure_modifiers());
        assert_eq!(
            parse("ctrl+win+shift").unwrap().display(),
            "Ctrl + Shift + Win"
        );
        // 写法顺序不影响结果（主人手写配置时可能按任意顺序写）
        assert_eq!(parse("win+ctrl").unwrap().to_config(), "ctrl+win");
    }

    /// egui 0.35 会为左右 Ctrl / Win 发出按键事件，捕捉时必须把它们
    /// **当修饰键跳过**，而不是当成"不支持的主键"。
    ///
    /// 认错的表现很隐蔽：主人刚按下 Ctrl 的瞬间捕捉就报错并清空半截状态，
    /// 于是 Ctrl+Win 永远录不出来 —— 这正是主人报的那个 bug。
    #[test]
    fn egui_modifier_variants_are_recognized() {
        for key in [
            egui::Key::ShiftLeft,
            egui::Key::ShiftRight,
            egui::Key::ControlLeft,
            egui::Key::ControlRight,
            egui::Key::AltLeft,
            egui::Key::AltRight,
            egui::Key::SuperLeft,
            egui::Key::SuperRight,
        ] {
            assert!(is_modifier_key(key), "{key:?} 是修饰键，不能被当成主键");
        }
        assert!(!is_modifier_key(egui::Key::A));
        assert!(!is_modifier_key(egui::Key::PageUp));
        // 修饰键也不能被当成"支持的主键"，否则 Ctrl 会被写进组合的主键位
        assert_eq!(egui_key_to_vk(egui::Key::ControlLeft), None);
        assert_eq!(egui_key_to_vk(egui::Key::SuperLeft), None);
    }

    /// `CURRENT_MODS` 的位打包必须能被解包还原成同一个组合
    /// （打包/解包写偏一个位，就会出现"保存后快捷键设置看着对、实际不生效"）
    #[test]
    fn mods_bits_round_trips_through_unpack() {
        for bits in 0u32..16 {
            let (ctrl, alt, shift, win) = unpack_mods(bits);
            let hk = Hotkey {
                ctrl,
                alt,
                shift,
                win,
                vk: 0x31,
            };
            assert_eq!(mods_bits(&hk), bits, "位 {bits:04b} 打包回去不一致");
        }
    }

    const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
        win: false,
    };
    const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        win: false,
    };
    /// 默认组合（Ctrl+Win）要求的修饰键状态
    const CTRL_WIN: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
        win: true,
    };
    /// 只按着 Win
    const WIN_ONLY: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        win: true,
    };

    /// 什么都没按的初始状态
    fn idle() -> Hold {
        Hold::default()
    }

    /// 把上一次的决策结果接着喂给下一次 —— 钩子里就是这么在事件之间传递状态的
    fn next(d: Decision) -> Hold {
        Hold {
            armed: d.armed,
            held_vk: d.held_vk,
            pending_win: d.pending_win,
            session_win: d.session_win,
        }
    }

    #[test]
    fn press_and_release_triggers_start_then_stop() {
        let hk = parse("ctrl+1").unwrap();
        // 按下 1
        let d = decide(hk, CTRL, 0x31, true, idle());
        assert_eq!(d.trigger, Some(Trigger::Start));
        assert!(d.swallow, "命中组合必须吞键，否则浏览器会切标签");
        assert!(d.armed);
        // 长按重复 keydown 不再重复开始
        let d2 = decide(hk, CTRL, 0x31, true, next(d));
        assert_eq!(d2.trigger, None);
        // 松开 1
        let d3 = decide(hk, CTRL, 0x31, false, next(d2));
        assert_eq!(d3.trigger, Some(Trigger::Stop));
        assert!(d3.swallow, "吞过 keydown 的键，keyup 也要吞");
        assert!(!d3.armed);
    }

    #[test]
    fn user_backtick_hotkey_works_with_ctrl_held() {
        let hk = parse("ctrl+`").unwrap();
        let start = decide(hk, CTRL, 0xC0, true, idle());
        assert_eq!(start.trigger, Some(Trigger::Start));
        let stop = decide(hk, CTRL, 0xC0, false, next(start));
        assert_eq!(stop.trigger, Some(Trigger::Stop));
        assert!(stop.swallow);
    }

    /// 默认组合「Ctrl + Win」的**常规**按法（先 Ctrl 后 Win）。
    ///
    /// 覆盖三件必须同时成立的事：
    /// 1. 只按 Ctrl 必须放行 —— 否则 Ctrl+C 这类全废；
    /// 2. 凑齐 Ctrl+Win 的那次按下必须**吞掉** —— 放行 Win 就会弹出开始菜单抢走焦点，
    ///    字根本打不进主人正在用的程序；
    /// 3. 长按不放时系统重复发 keydown，必须继续吞；
    /// 4. 松开时吞掉 Win 的 keyup，且**不补发**任何东西（补发=弹开始菜单）。
    #[test]
    fn pure_modifier_combo_ctrl_win() {
        let hk = parse("ctrl+win").unwrap();
        assert!(hk.is_pure_modifiers(), "ctrl+win 不该有主键");
        assert_eq!(hk.display(), "Ctrl + Win");
        assert_eq!(hk.to_config(), "ctrl+win");

        // 先按 Ctrl（组合还没凑齐）：放行
        let ctrl_only = decide(hk, CTRL, 0xA2, true, idle());
        assert_eq!(ctrl_only.trigger, None);
        assert!(
            !ctrl_only.swallow,
            "组合没凑齐时按 Ctrl 不能吞（会影响 Ctrl+C 等）"
        );

        // 再按 Win：凑齐 → 开始录音，并且必须吞掉 Win
        let start = decide(hk, CTRL_WIN, 0x5B, true, next(ctrl_only));
        assert_eq!(start.trigger, Some(Trigger::Start));
        assert!(start.swallow, "必须吞掉 Win，否则开始菜单会弹出来抢焦点");
        assert_eq!(start.held_vk, 0x5B, "要记住吞的是 Win，它的 keyup 也得吞");
        assert!(start.armed);

        // 长按重复 keydown：继续吞、不重复触发
        let repeat = decide(hk, CTRL_WIN, 0x5B, true, next(start));
        assert_eq!(repeat.trigger, None, "长按不能重复开始录音");
        assert!(repeat.swallow, "长按时重复的 Win keydown 也必须继续吞");

        // 松开 Win：结束录音，吞掉 Win 的 keyup，且不补发
        let stop = decide(hk, CTRL, 0x5B, false, next(repeat));
        assert_eq!(stop.trigger, Some(Trigger::Stop));
        assert!(stop.swallow, "Win 的 keydown 吞了，keyup 也得吞，否则会弹开始菜单");
        assert_eq!(stop.replay, Replay::None, "这里补发就等于把开始菜单弹出来");
        assert!(!stop.armed);
        assert_eq!(stop.held_vk, 0, "Win 的 keyup 已经处理过，不该再留着");
    }

    /// **主人报的那个 bug 的回归点**：先按 Win、后按 Ctrl（顺序不巧）。
    ///
    /// 旧实现里 Win 的按下会原样放给外壳（那时组合还没凑齐），而松开时又不吞
    /// （记账已经被 Ctrl 顶掉了）——外壳于是看到了一次**完整、独立**的 Win 按键，
    /// 开始菜单随之弹出抢走焦点，表现就是主人说的
    /// "按住 Ctrl+Win 说话时突然断开，然后像只按着 Win 那样弹出开始界面"。
    ///
    /// 现在：Win 的按下被接管 → 凑齐时收编 → 全程没有任何一次 Win 事件落到外壳，
    /// 所以整条路径上**一次补发都不该出现**（补发就是弹开始菜单）。
    #[test]
    fn win_pressed_before_ctrl_never_leaks_to_the_shell() {
        let hk = parse("ctrl+win").unwrap();

        // 先按 Win：只记下来，还不补发（主人可能正要按 Ctrl）
        let win_down = decide(hk, WIN_ONLY, 0x5B, true, idle());
        assert_eq!(win_down.trigger, None, "只按 Win 不能开始录音");
        assert!(win_down.swallow, "Win 的按下必须接管");
        assert_eq!(win_down.pending_win, 0x5B);
        assert_eq!(win_down.replay, Replay::None);

        // 再按 Ctrl：组合凑齐 → 开始录音，那次 Win 被收编进本次按住
        let start = decide(hk, CTRL_WIN, 0xA2, true, next(win_down));
        assert_eq!(start.trigger, Some(Trigger::Start));
        assert!(start.swallow);
        assert_eq!(start.pending_win, 0, "已经收编，不再欠 Win 的账");
        assert_eq!(start.session_win, 0x5B);

        // 说完先松 Ctrl：结束录音（Ctrl 的 keydown 也是我们吞的，keyup 同样要吞）
        let ctrl_up = decide(hk, WIN_ONLY, 0xA2, false, next(start));
        assert_eq!(ctrl_up.trigger, Some(Trigger::Stop));
        assert!(ctrl_up.swallow);
        assert_eq!(ctrl_up.session_win, 0x5B, "Win 还按着，记账不能丢");

        // 再松 Win：吞掉，并且**不补发** —— 这里一补发就开始菜单
        let win_up = decide(hk, NONE, 0x5B, false, next(ctrl_up));
        assert_eq!(win_up.trigger, None, "不该重复结束录音");
        assert!(win_up.swallow, "收了 Win 的按下，它的松开也得吞");
        assert_eq!(
            win_up.replay,
            Replay::None,
            "补发一套 Win 按键 = 弹出开始菜单，这正是要修的那个 bug"
        );
        assert_eq!(win_up.session_win, 0, "处理完就该把记账清掉");
    }

    /// 单独按 Win（想打开开始菜单）必须照常可用 —— 接管 Win 的前提是"别把系统弄坏"。
    ///
    /// 做法：吞掉原始的 Win 按下与松开，补一整套 Win 按下+松开。这条是拿注入探针
    /// 实测过的（补发之后开始菜单正常弹出，系统里 Win 的状态也干净）。
    #[test]
    fn win_alone_still_opens_the_start_menu() {
        let hk = parse("ctrl+win").unwrap();
        let down = decide(hk, WIN_ONLY, 0x5B, true, idle());
        assert!(down.swallow);
        assert_eq!(down.pending_win, 0x5B);

        let up = decide(hk, NONE, 0x5B, false, next(down));
        assert_eq!(up.trigger, None, "单独按 Win 不是快捷键");
        assert!(up.swallow, "原始的松开吞掉，改由补发的那一套顶上去");
        assert_eq!(
            up.replay,
            Replay::WinPress(0x5B),
            "单独按 Win 必须照常弹出开始菜单"
        );
        assert_eq!(up.pending_win, 0);
    }

    /// Win+X（Win+E 这类系统组合键）必须还能用，而且**必须一次提交**。
    ///
    /// 为什么不能"先把当前键放行、再补 Win"：外壳会先收到那个键（它还以为 Win
    /// 没按下），组合键失效、字母被当普通字符打出去 —— 探针实测过。
    #[test]
    fn win_plus_key_is_replayed_in_one_batch() {
        let hk = parse("ctrl+win").unwrap();
        let down = decide(hk, WIN_ONLY, 0x5B, true, idle());

        // Win+E：吞掉 E 的原始按下，改为一次性补发「Win↓ + E↓」
        let key = decide(hk, WIN_ONLY, 0x45, true, next(down));
        assert_eq!(key.trigger, None);
        assert_eq!(key.replay, Replay::WinThenCurrentKey(0x5B));
        assert!(key.swallow, "原始按下要吞掉，否则会多打一个字符");
        assert_eq!(key.pending_win, 0);

        // 松开 E、松开 Win 都原样放行：外壳已经拿到完整的一次 Win+E
        let e_up = decide(hk, NONE, 0x45, false, next(key));
        assert!(!e_up.swallow, "E 的按下我们没吞（补发的是另一份），松开要放行");
        let win_up = decide(hk, NONE, 0x5B, false, next(e_up));
        assert!(!win_up.swallow, "外壳看过完整的 Win 按键，松开必须放行");
        assert_eq!(win_up.replay, Replay::None, "再补发一次就会多弹开始菜单");
    }

    /// 长按 Win 的自动重复不能把记账冲掉：松开时仍要补发一次完整的 Win 按键
    #[test]
    fn win_repeat_while_pending_keeps_the_record() {
        let hk = parse("ctrl+win").unwrap();
        let mut hold = next(decide(hk, WIN_ONLY, 0x5B, true, idle()));
        for _ in 0..5 {
            let d = decide(hk, WIN_ONLY, 0x5B, true, hold);
            assert!(d.swallow, "自动重复的 Win keydown 也必须吞");
            assert_eq!(d.pending_win, 0x5B, "记账被重复事件冲掉了");
            assert_eq!(d.replay, Replay::None, "还没松开，不能补发");
            hold = next(d);
        }
        let up = decide(hk, NONE, 0x5B, false, hold);
        assert_eq!(up.replay, Replay::WinPress(0x5B));
    }

    /// 记账万一成了"半截"（换键、改键捕捉结束的时刻正好撞上按键事件，清理与写回
    /// 挨在一起），也绝不能凭一笔过期的账补发系统组合键 —— 那等于凭空替主人按了
    /// Win+X。判据很简单：那次 Win 必须**现在还按着**。
    #[test]
    fn stale_pending_account_cannot_fire_a_replay() {
        let hk = parse("ctrl+win").unwrap();
        let stale = Hold {
            pending_win: 0x5B,
            ..Hold::default()
        };
        // Win 其实早就松开了（mods.win = false），此时按下别的键
        let d = decide(hk, NONE, 0x45, true, stale);
        assert_eq!(d.replay, Replay::None, "过期的记账绝不能补出系统组合键");
        assert!(!d.swallow, "普通按键还得原样放行");
    }

    /// 录音途中主人随手按了个字母：原样放行（不能吞、更不能补发 Win）
    #[test]
    fn stray_key_during_a_session_passes_through() {
        let hk = parse("ctrl+win").unwrap();
        let win = decide(hk, WIN_ONLY, 0x5B, true, idle());
        let start = decide(hk, CTRL_WIN, 0xA2, true, next(win));
        assert!(start.armed && start.pending_win == 0, "收编之后不再欠 Win 的账");
        let stray = decide(hk, CTRL_WIN, 0x45, true, next(start));
        assert!(!stray.swallow, "录音途中的普通按键要放行");
        assert_eq!(stray.replay, Replay::None);
        assert!(stray.armed, "不该打断本次录音");
    }

    /// 回归（比原 bug 更烦人的那一种）：录音途中又按了组合外的修饰键（组合因此
    /// 不匹配），此时 Win 的**长按自动重复**到达 —— 它只是重复，绝不能记成
    /// "欠一次补发"。记了的话，主人随后随手按的字母会被补发成 Win+X
    /// （打开资源管理器、切换桌面之类），等于凭空替他按了系统快捷键。
    #[test]
    fn win_repeat_during_a_session_cannot_arm_a_replay() {
        let hk = parse("ctrl+win").unwrap();
        let win = decide(hk, WIN_ONLY, 0x5B, true, idle());
        let start = decide(hk, CTRL_WIN, 0xA2, true, next(win));
        assert!(start.armed);

        // 组合外又按下 Shift：mods_ok 从此不成立
        let with_shift = Mods {
            shift: true,
            ..CTRL_WIN
        };
        let shift_down = decide(hk, with_shift, 0xA0, true, next(start));
        assert!(shift_down.armed, "多按一个修饰键不该中断录音");

        // Win 的自动重复：继续吞，但不许攒账
        let repeat = decide(hk, with_shift, 0x5B, true, next(shift_down));
        assert!(repeat.swallow, "录音途中 Win 的重复事件也必须吞掉");
        assert_eq!(repeat.pending_win, 0, "这里攒账，后面就会凭空补发 Win+X");
        assert_eq!(repeat.replay, Replay::None);

        // 随后随手按个字母：原样放行，绝不能被当成 Win+X 补发出去
        let stray = decide(hk, with_shift, 0x58, true, next(repeat));
        assert_eq!(stray.replay, Replay::None, "录音途中绝不许补发组合键");
        assert!(!stray.swallow, "普通按键要原样放行");
        assert!(stray.armed);
    }

    /// 组合里没有 Win 时，Win 键和我们一点关系都没有（不许吞、不许补发）
    #[test]
    fn hotkey_without_win_does_not_touch_the_win_key() {
        let hk = parse("ctrl+1").unwrap();
        let down = decide(hk, NONE, 0x5B, true, idle());
        assert!(!down.swallow, "组合里没有 Win 就不许碰它，否则开始菜单打不开");
        assert_eq!(down.pending_win, 0);
        let up = decide(hk, NONE, 0x5B, false, next(down));
        assert!(!up.swallow);
        assert_eq!(up.replay, Replay::None);
    }

    /// 纯修饰键组合里"先松 Ctrl 再松 Win"（大家都这么松手）：
    /// Ctrl 的 keydown 从没被吞过，它的 keyup 绝不能吞；而 Win 的 keyup 仍要吞 ——
    /// `held_vk` 记的是具体键码而不是 true/false，就是为了这一条。
    #[test]
    fn releasing_ctrl_before_win_still_swallows_win_keyup() {
        let hk = parse("ctrl+win").unwrap();
        let start = decide(hk, CTRL_WIN, 0x5B, true, idle());
        assert_eq!(start.trigger, Some(Trigger::Start));

        // 先松 Ctrl：结束录音，但 Ctrl 的 keyup 必须放行
        let ctrl_up = decide(hk, WIN_ONLY, 0xA2, false, next(start));
        assert_eq!(ctrl_up.trigger, Some(Trigger::Stop));
        assert!(!ctrl_up.swallow, "Ctrl 的 keydown 放行过，keyup 也必须放行");
        assert_eq!(ctrl_up.held_vk, 0x5B, "还欠着 Win 的 keyup，不能把记录清掉");

        // 再松 Win：不重复结束，但 keyup 必须吞掉
        let win_up = decide(hk, NONE, 0x5B, false, next(ctrl_up));
        assert_eq!(win_up.trigger, None, "不该重复发结束录音");
        assert!(win_up.swallow, "Win 的 keydown 吞了，keyup 必须也吞");
        assert_eq!(win_up.held_vk, 0);
    }

    /// 组合外的修饰键不能干扰：Ctrl+Win 正在录音时又按下 Shift，
    /// 松开 Shift 不该结束录音（Shift 不在组合里）
    #[test]
    fn extra_modifier_release_does_not_stop_recording() {
        let hk = parse("ctrl+win").unwrap();
        let start = decide(hk, CTRL_WIN, 0x5B, true, idle());
        let with_shift = Mods {
            shift: true,
            ..CTRL_WIN
        };
        // 按下 Shift：组合已经不匹配，放行（不结束、也不吞）
        let shift_down = decide(hk, with_shift, 0xA0, true, next(start));
        assert!(!shift_down.swallow);
        assert!(shift_down.armed, "多按一个修饰键不该中断录音");
        // 松开 Shift：仍然不该结束录音
        let shift_up = decide(hk, CTRL_WIN, 0xA0, false, next(shift_down));
        assert_eq!(shift_up.trigger, None, "组合外的修饰键松开不该结束录音");
        assert!(shift_up.armed);
    }

    #[test]
    fn extra_modifier_does_not_trigger() {
        let hk = parse("ctrl+1").unwrap();
        let with_shift = Mods {
            shift: true,
            ..CTRL
        };
        assert_eq!(decide(hk, with_shift, 0x31, true, idle()).trigger, None);
        assert!(!decide(hk, with_shift, 0x31, true, idle()).swallow);
    }

    #[test]
    fn releasing_modifier_first_still_stops() {
        let hk = parse("ctrl+1").unwrap();
        let start = decide(hk, CTRL, 0x31, true, idle());
        assert_eq!(start.trigger, Some(Trigger::Start));
        // 用户先松开 Ctrl
        let stop = decide(hk, NONE, 0xA2, false, next(start));
        assert_eq!(stop.trigger, Some(Trigger::Stop));
        assert!(!stop.armed);
        // 之后再松开 1：不重复发 Stop，但 keyup 仍要吞（keydown 没放行过）
        let after = decide(hk, NONE, 0x31, false, next(stop));
        assert_eq!(after.trigger, None);
        assert!(after.swallow, "先松修饰键时，触发键的 keyup 仍必须吞");
    }

    /// 回归测试（按键卡住 bug）：没按修饰键直接按触发键（如只按 1）时，
    /// keydown 已放行给前台程序，keyup 也必须放行——
    /// 否则在游戏等跟踪 keyup 的程序里表现为按键一直被按住。
    #[test]
    fn bare_trigger_keyup_passes_through() {
        let hk = parse("ctrl+1").unwrap();
        let down = decide(hk, NONE, 0x31, true, idle());
        assert!(!down.swallow, "裸按触发键：keydown 必须放行");
        assert_eq!(down.trigger, None);
        let up = decide(hk, NONE, 0x31, false, next(down));
        assert!(!up.swallow, "裸按触发键：keyup 也必须放行，否则按键卡住");
        assert_eq!(up.trigger, None);
    }

    /// 真实键盘上报的是左右分开的虚拟键码（左 Ctrl = 0xA2），一个都不能漏
    #[test]
    fn all_modifier_variants_are_classified() {
        use ModKind::*;
        let cases = [
            (0x11, Ctrl),
            (0xA2, Ctrl),
            (0xA3, Ctrl),
            (0x12, Alt),
            (0xA4, Alt),
            (0xA5, Alt),
            (0x10, Shift),
            (0xA0, Shift),
            (0xA1, Shift),
            (0x5B, Win),
            (0x5C, Win),
        ];
        for (vk, kind) in cases {
            assert_eq!(
                classify_modifier(vk),
                Some(kind),
                "0x{vk:X} 应被识别为 {kind:?}"
            );
        }
        assert_eq!(classify_modifier(0x31), None, "普通键不算修饰键");
    }

    #[test]
    fn unrelated_keys_pass_through_untouched() {
        let hk = parse("ctrl+1").unwrap();
        let d = decide(hk, CTRL, 0x41, true, idle()); // Ctrl+A
        assert_eq!(d.trigger, None);
        assert!(!d.swallow, "非组合键必须放行");
    }

    /// 回归测试（满屏 1111 那个 bug）：
    /// 长按触发键时系统会不断发 keydown，只要修饰键状态还正确，
    /// 这些重复事件必须全部被吞掉，绝不能漏给前台程序。
    #[test]
    fn repeated_keydowns_while_holding_are_all_swallowed() {
        let hk = parse("ctrl+1").unwrap();
        let first = decide(hk, CTRL, 0x31, true, idle());
        assert_eq!(first.trigger, Some(Trigger::Start));
        assert!(first.swallow);

        // 模拟长按产生的 20 次自动重复
        let mut hold = next(first);
        for _ in 0..20 {
            let repeat = decide(hk, CTRL, 0x31, true, hold);
            assert!(
                repeat.swallow,
                "自动重复的 keydown 必须继续吞掉，否则会出现 1111"
            );
            assert_eq!(repeat.trigger, None, "不能重复触发开始录音");
            hold = next(repeat);
        }

        let stop = decide(hk, CTRL, 0x31, false, hold);
        assert_eq!(stop.trigger, Some(Trigger::Stop));
    }

    /// 右 Win（0x5C）和左 Win 一样要接管 —— 有人就是按右边的
    #[test]
    fn right_win_is_taken_over_too() {
        let hk = parse("ctrl+win").unwrap();
        let down = decide(hk, WIN_ONLY, 0x5C, true, idle());
        assert!(down.swallow);
        assert_eq!(down.pending_win, 0x5C);
        let up = decide(hk, NONE, 0x5C, false, next(down));
        assert_eq!(up.replay, Replay::WinPress(0x5C), "补发要补同一个键");
    }

    /// 修饰键状态必须左右键码一视同仁（状态失真会导致组合键失灵）
    #[test]
    fn apply_mods_tracks_press_and_release() {
        let down = apply_mods(NONE, 0xA2, true); // 物理左 Ctrl
        assert!(down.ctrl);
        let up = apply_mods(down, 0xA2, false);
        assert_eq!(up, NONE, "松开后必须回到未按下的状态");
        // 普通键不该动修饰键状态
        assert_eq!(apply_mods(CTRL, 0x31, true), CTRL);
    }

    /// 为什么 `set_current` 必须清掉 armed：换键后旧键的 keyup 既不命中新键、
    /// 也不是修饰键，armed 会永久挂着 —— 会话收不到 Stop，录音停不下来。
    /// 这里刻意不调用 set_current：它是进程级静态变量，单测里改它会污染并行跑的其他用例。
    #[test]
    fn stale_armed_state_is_why_set_current_resets_it() {
        let old = parse("ctrl+1").unwrap();
        let start = decide(old, CTRL, 0x31, true, idle());
        assert_eq!(start.trigger, Some(Trigger::Start));

        // 录音途中改成 Ctrl+2，然后松开旧键 1
        let new = parse("ctrl+2").unwrap();
        let stale = decide(new, CTRL, 0x31, false, next(start));
        assert_eq!(stale.trigger, None, "旧键的 keyup 不会命中新键");
        assert!(
            stale.armed,
            "armed 会一直挂着，这正是 set_current 要清掉它的原因"
        );

        // armed 被清掉之后，新组合能正常工作
        assert_eq!(
            decide(new, CTRL, 0x32, true, idle()).trigger,
            Some(Trigger::Start)
        );
    }

    /// 状态机的两条不变量（这次改动最容易踩坏的地方，单独钉住）：
    /// 1. 「按住中」时绝不会还欠着 Win 的账 —— 凑齐那一刻就收编了；
    /// 2. 补发只可能发生在"主人按的确实是 Win 相关按键"时，绝不会凭空补。
    #[test]
    fn hold_invariants_after_every_event_of_both_orders() {
        let hk = parse("ctrl+win").unwrap();
        // 先 Win 后 Ctrl
        let a = decide(hk, WIN_ONLY, 0x5B, true, idle());
        let b = decide(hk, CTRL_WIN, 0xA2, true, next(a));
        let c = decide(hk, WIN_ONLY, 0x5B, false, next(b));
        let d = decide(hk, NONE, 0xA2, false, next(c));
        for step in [a, b, c, d] {
            assert!(!(step.armed && step.pending_win != 0), "armed 时不该欠 Win 的账");
        }
        // 先 Ctrl 后 Win
        let a = decide(hk, CTRL, 0xA2, true, idle());
        let b = decide(hk, CTRL_WIN, 0x5B, true, next(a));
        let c = decide(hk, CTRL, 0x5B, false, next(b));
        let d = decide(hk, NONE, 0xA2, false, next(c));
        for step in [a, b, c, d] {
            assert!(!(step.armed && step.pending_win != 0), "armed 时不该欠 Win 的账");
            assert_eq!(step.replay, Replay::None, "这条路径上不该有任何补发");
        }
    }
}
