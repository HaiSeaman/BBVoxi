//! 全局快捷键：WH_KEYBOARD_LL 低级键盘钩子，独立线程跑消息循环。
//! 用钩子而不是 RegisterHotKey，因为按住说话需要「按下」和「松开」两个时刻。
//! 命中组合键时会吞掉事件（否则 Ctrl+1 会触发浏览器切标签）。

use anyhow::{bail, Result};
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
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            win: false,
            vk: 0x31, // 1
        }
    }
}

impl Hotkey {
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
        parts.push(vk_name(self.vk));
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
        parts.push(vk_name(self.vk).to_lowercase());
        parts.join("+")
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Esc".into(),
        0x08 => "Backspace".into(),
        0xC0 => "`".into(),
        v if (0x30..=0x39).contains(&v) => ((b'0' + (v - 0x30) as u8) as char).to_string(),
        v if (0x41..=0x5A).contains(&v) => ((b'A' + (v - 0x41) as u8) as char).to_string(),
        v if (0x70..=0x87).contains(&v) => format!("F{}", v - 0x70 + 1),
        v => format!("VK{v:02X}"),
    }
}

/// 解析 "ctrl+1" / "alt+space" / "f9" / "ctrl+shift+f9"
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
    let Some(vk) = key else {
        bail!("缺少主键，例如 ctrl+1");
    };
    hk.vk = vk;
    if !(hk.ctrl || hk.alt || hk.shift || hk.win) && !(0x70..=0x87).contains(&vk) {
        bail!("至少需要一个修饰键（Ctrl/Alt/Shift/Win），或使用 F1~F24");
    }
    Ok(hk)
}

fn key_vk(t: &str) -> Result<u32> {
    let vk = match t {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "`" | "backtick" | "grave" => 0xC0,
        "-" | "minus" => 0xBD,
        "=" | "equal" => 0xBB,
        "[" | "bracketleft" => 0xDB,
        "]" | "bracketright" => 0xDD,
        "\\" | "backslash" => 0xDC,
        ";" | "semicolon" => 0xBA,
        "'" | "quote" => 0xDE,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        "/" | "slash" => 0xBF,
        one if one.len() == 1 => {
            let c = one.chars().next().unwrap();
            match c {
                '0'..='9' => 0x30 + (c as u32 - '0' as u32),
                'a'..='z' => 0x41 + (c as u32 - 'a' as u32),
                _ => bail!("不支持的按键：{t}"),
            }
        }
        f if f.starts_with('f') => {
            let n: u32 = f[1..]
                .parse()
                .map_err(|_| anyhow::anyhow!("不支持的按键：{t}"))?;
            if !(1..=24).contains(&n) {
                bail!("功能键只支持 F1~F24：{t}");
            }
            0x70 + n - 1
        }
        _ => bail!("不支持的按键：{t}"),
    };
    Ok(vk)
}

struct HookCtx {
    tx: UnboundedSender<Cmd>,
    paused: Arc<AtomicBool>,
    ctrl: AtomicBool,
    alt: AtomicBool,
    shift: AtomicBool,
    win: AtomicBool,
    armed: AtomicBool,
}

static CTX: OnceLock<HookCtx> = OnceLock::new();

// 当前生效的组合键，改动后立即生效（不必重启）
static CURRENT_VK: AtomicU32 = AtomicU32::new(0x31);
static CURRENT_MODS: AtomicU32 = AtomicU32::new(1); // bit0 Ctrl / bit1 Alt / bit2 Shift / bit3 Win

pub fn set_current(hk: Hotkey) {
    CURRENT_VK.store(hk.vk, Ordering::Relaxed);
    CURRENT_MODS.store(mods_bits(&hk), Ordering::Relaxed);
}

pub fn current() -> Hotkey {
    let m = CURRENT_MODS.load(Ordering::Relaxed);
    Hotkey {
        ctrl: m & 1 != 0,
        alt: m & 2 != 0,
        shift: m & 4 != 0,
        win: m & 8 != 0,
        vk: CURRENT_VK.load(Ordering::Relaxed),
    }
}

fn mods_bits(hk: &Hotkey) -> u32 {
    (hk.ctrl as u32) | ((hk.alt as u32) << 1) | ((hk.shift as u32) << 2) | ((hk.win as u32) << 3)
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
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
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
    let injected = kb.flags.0 & LLKHF_INJECTED != 0;
    // 双保险：除了系统标记，还看我们自己的 dwExtraInfo 标记
    let ours = kb.dwExtraInfo == crate::injector::INJECT_TAG;
    let vk = kb.vkCode;
    let msg = wparam.0 as u32;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;

    // 注入事件必须完全忽略：我们自己在打字前会注入修饰键 KEYUP（把 Ctrl「清掉」），
    // 如果让它参与状态跟踪，钩子会误判成"用户松开了 Ctrl"，
    // 于是触发键不再被吞 → 前台程序收到 1 的自动重复，出现满屏 111111。
    if ignore_event(injected || ours, ctx.paused.load(Ordering::Relaxed)) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // 修饰键状态跟着事件走（左右变体由 classify_modifier 统一处理）
    match classify_modifier(vk) {
        Some(ModKind::Ctrl) => ctx.ctrl.store(down, Ordering::Relaxed),
        Some(ModKind::Alt) => ctx.alt.store(down, Ordering::Relaxed),
        Some(ModKind::Shift) => ctx.shift.store(down, Ordering::Relaxed),
        Some(ModKind::Win) => ctx.win.store(down, Ordering::Relaxed),
        None => {}
    }

    {
        let mods = Mods {
            ctrl: ctx.ctrl.load(Ordering::Relaxed),
            alt: ctx.alt.load(Ordering::Relaxed),
            shift: ctx.shift.load(Ordering::Relaxed),
            win: ctx.win.load(Ordering::Relaxed),
        };
        let decision = decide(current(), mods, vk, down, ctx.armed.load(Ordering::Relaxed));
        ctx.armed.store(decision.armed, Ordering::Relaxed);

        match decision.trigger {
            Some(Trigger::Start) => {
                crate::log::log("快捷键按下 → 开始录音");
                let _ = ctx.tx.send(Cmd::Start);
            }
            Some(Trigger::Stop) => {
                crate::log::log("快捷键松开 → 结束录音");
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

/// 完全不处理这个事件？（自己注入的事件 / 设置界面正在录制快捷键时）
pub fn ignore_event(injected: bool, paused: bool) -> bool {
    injected || paused
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub trigger: Option<Trigger>,
    /// 是否吞掉这个按键（命中组合键时吞掉，避免 Ctrl+1 触发浏览器切标签）
    pub swallow: bool,
    /// 处理之后是否处于「按住中」状态
    pub armed: bool,
}

/// 判定逻辑抽成纯函数：钩子回调本身没法单元测试，
/// 而「按住说话」的正确性全靠这里（这块之前没人看着，就出过问题）。
pub fn decide(hk: Hotkey, mods: Mods, vk: u32, down: bool, armed: bool) -> Decision {
    let mods_ok =
        hk.ctrl == mods.ctrl && hk.alt == mods.alt && hk.shift == mods.shift && hk.win == mods.win;

    if down {
        if vk == hk.vk && mods_ok {
            return Decision {
                // 长按时系统会重复发 keydown，不能重复触发
                trigger: if armed { None } else { Some(Trigger::Start) },
                swallow: true,
                armed: true,
            };
        }
        return Decision {
            trigger: None,
            swallow: false,
            armed,
        };
    }

    if vk == hk.vk {
        return Decision {
            trigger: if armed { Some(Trigger::Stop) } else { None },
            swallow: true,
            armed: false,
        };
    }
    // 先松开修饰键（例如先放 Ctrl 再放 1）也要结束录音
    if armed && is_modifier(vk) {
        return Decision {
            trigger: Some(Trigger::Stop),
            swallow: false,
            armed: false,
        };
    }
    Decision {
        trigger: None,
        swallow: false,
        armed,
    }
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
        assert!(parse("ctrl").is_err()); // 缺主键
        assert!(parse("ctrl+a+b").is_err()); // 多个主键
        assert!(parse("ctrl+f99").is_err());
        assert!(parse("f9").is_ok()); // 功能键可单独用
    }

    /// 用户实际用的组合：Ctrl + 反引号（曾经因为"保存后没同步给钩子"而失效）
    #[test]
    fn applies_user_hotkey_immediately() {
        let hk = apply_from_config("ctrl+`").unwrap();
        assert_eq!(hk.vk, 0xC0);
        assert!(hk.ctrl);
        assert_eq!(current(), hk, "钩子里生效的必须就是刚设置的组合");
        assert_eq!(hk.to_config(), "ctrl+`");
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

    #[test]
    fn press_and_release_triggers_start_then_stop() {
        let hk = parse("ctrl+1").unwrap();
        // 按下 1
        let d = decide(hk, CTRL, 0x31, true, false);
        assert_eq!(d.trigger, Some(Trigger::Start));
        assert!(d.swallow, "命中组合必须吞键，否则浏览器会切标签");
        assert!(d.armed);
        // 长按重复 keydown 不再重复开始
        let d2 = decide(hk, CTRL, 0x31, true, d.armed);
        assert_eq!(d2.trigger, None);
        // 松开 1
        let d3 = decide(hk, CTRL, 0x31, false, d2.armed);
        assert_eq!(d3.trigger, Some(Trigger::Stop));
        assert!(d3.swallow);
        assert!(!d3.armed);
    }

    #[test]
    fn user_backtick_hotkey_works_with_ctrl_held() {
        let hk = parse("ctrl+`").unwrap();
        let start = decide(hk, CTRL, 0xC0, true, false);
        assert_eq!(start.trigger, Some(Trigger::Start));
        let stop = decide(hk, CTRL, 0xC0, false, start.armed);
        assert_eq!(stop.trigger, Some(Trigger::Stop));
    }

    #[test]
    fn extra_modifier_does_not_trigger() {
        let hk = parse("ctrl+1").unwrap();
        let with_shift = Mods {
            shift: true,
            ..CTRL
        };
        assert_eq!(decide(hk, with_shift, 0x31, true, false).trigger, None);
        assert!(!decide(hk, with_shift, 0x31, true, false).swallow);
    }

    #[test]
    fn releasing_modifier_first_still_stops() {
        let hk = parse("ctrl+1").unwrap();
        let start = decide(hk, CTRL, 0x31, true, false);
        assert_eq!(start.trigger, Some(Trigger::Start));
        // 用户先松开 Ctrl
        let stop = decide(hk, NONE, 0xA2, false, start.armed);
        assert_eq!(stop.trigger, Some(Trigger::Stop));
        assert!(!stop.armed);
        // 之后再松开 1 不应该重复发 Stop
        assert_eq!(decide(hk, NONE, 0x31, false, false).trigger, None);
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
        let d = decide(hk, CTRL, 0x41, true, false); // Ctrl+A
        assert_eq!(d.trigger, None);
        assert!(!d.swallow, "非组合键必须放行");
    }

    /// 回归测试（满屏 1111 那个 bug）：
    /// 长按触发键时系统会不断发 keydown，只要修饰键状态还正确，
    /// 这些重复事件必须全部被吞掉，绝不能漏给前台程序。
    #[test]
    fn repeated_keydowns_while_holding_are_all_swallowed() {
        let hk = parse("ctrl+1").unwrap();
        let first = decide(hk, CTRL, 0x31, true, false);
        assert_eq!(first.trigger, Some(Trigger::Start));
        assert!(first.swallow);

        // 模拟长按产生的 20 次自动重复
        let mut armed = first.armed;
        for _ in 0..20 {
            let repeat = decide(hk, CTRL, 0x31, true, armed);
            assert!(
                repeat.swallow,
                "自动重复的 keydown 必须继续吞掉，否则会出现 1111"
            );
            assert_eq!(repeat.trigger, None, "不能重复触发开始录音");
            armed = repeat.armed;
        }

        let stop = decide(hk, CTRL, 0x31, false, armed);
        assert_eq!(stop.trigger, Some(Trigger::Stop));
    }

    /// 注入事件必须完全忽略：自己的「修饰键重置」不能被当成用户松开 Ctrl
    #[test]
    fn injected_events_and_pause_are_ignored() {
        assert!(ignore_event(true, false), "注入事件必须忽略");
        assert!(ignore_event(false, true), "录制快捷键时必须忽略");
        assert!(!ignore_event(false, false));
    }
}
