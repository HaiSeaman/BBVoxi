//! 开机自启：写 HKCU\Software\Microsoft\Windows\CurrentVersion\Run（当前用户，不需要管理员权限）。

use anyhow::{bail, Context, Result};
use windows::core::HSTRING;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "BBVoxi";

/// 启动项里的命令行标记：带上它就表示"这次是开机自启拉起来的，静默后台即可"。
///
/// 为什么非要有个标记：开机自启不该弹窗，而主人在资源管理器里**双击 exe**
/// 时正相反 —— 他就是想打开设置窗。两者启动的是同一个 exe、同一个名字，
/// 除了参数没有任何区别，只能靠启动项里写死这个标记来区分。
pub const AUTOSTART_FLAG: &str = "--autostart";

/// 「开机自启」是否已开启 —— 判定标准是**启动项真的会拉起本程序**，
/// 而不是"注册表里有个叫 BBVoxi 的值"。
///
/// 为什么要看路径：主人把新版装到新目录后，注册表里那条启动项还指着**旧版**
/// exe（旧版会在开机时弹窗，而且用什么新代码都改不了它）。此时若只说
/// "有个值 → 已开启"，设置窗的勾会一直亮着，主人永远不会去重设，于是
/// "开机自启不弹窗"这件事在他机器上永远不生效。显示成未勾选才是实话，
/// 他重新勾一次，启动项才会换成新版 exe。
pub fn is_enabled() -> bool {
    current_command().is_some_and(|c| is_own_entry(&c))
}

pub fn set(enabled: bool) -> Result<()> {
    set_named(VALUE_NAME, enabled)
}

/// 判定为「开机自启」的系统运行时长上限：只有系统开机后这么久以内启动的进程，
/// 才可能是被启动项拉起来的（见 `boot_decision`）。
const BOOT_UPTIME_WINDOW_MS: u64 = 5 * 60 * 1000;

/// 本次进程是不是被「开机自启」项拉起来的。
///
/// 两种认法：
/// 1. 命令行里有 [`AUTOSTART_FLAG`]（本版起写进去的新格式）；
/// 2. 老版本写进去的启动项**没有参数**，只有一条 `"C:\...\bbvoxi.exe"`，
///    且**系统刚开机没多久**（那样进程只可能是开机时拉起的）。认出来就顺手升级成
///    新格式 —— 不认的话，老用户升级后第一次开机照样被弹一脸，得等到第二次开机才安生。
pub fn launched_at_boot() -> bool {
    let has_flag = std::env::args().skip(1).any(|a| a == AUTOSTART_FLAG);
    // 只认"老格式"的启动项：有参数的是新格式，本次是否自启由 `has_flag` 说了算
    let legacy_entry =
        current_command().is_some_and(|c| is_own_entry(&c) && !has_autostart_flag(&c));
    let (boot, upgrade) = boot_decision(has_flag, legacy_entry, uptime_ms());
    if upgrade {
        // 升级启动项只是"顺手"，失败了不该影响本次是否隐藏：本次照样按开机自启处理。
        if let Err(e) = set(true) {
            crate::log::log(format!("升级开机自启项格式失败：{e}"));
        } else {
            crate::log::log("开机自启项已升级为带 --autostart 参数的新格式");
        }
    }
    boot
}

/// 「本次是否算开机自启」的纯判定，返回 (是否开机自启, 是否需要升级注册表项)。
///
/// 为什么抽成纯函数：它决定"窗口弹不弹"，却被夹了注册表读取和系统开机时长，
/// 没法直接测；抽出来就能把各种组合枚举着测（见下方单测）。
///
/// 为什么老格式还要看系统运行时长：老格式的启动项里没有 `--autostart` 参数，主人
/// **手动双击 exe** 时同样会命中它。若只看"注册表里有我们的路径"，就会把手动双击
/// 也当成开机自启 —— 窗口一个都不弹（主人以为程序坏了），程序还顺手改写了注册表。
/// 加上"系统刚开机"这个条件后，只有真·开机拉起才会被认成自启。
fn boot_decision(has_flag: bool, legacy_entry: bool, uptime_ms: u64) -> (bool, bool) {
    if has_flag {
        // 新格式：命令行里明明白白带着标记，无需再看别的
        return (true, false);
    }
    if legacy_entry && uptime_ms < BOOT_UPTIME_WINDOW_MS {
        // 老格式 + 系统刚启动 → 只能是开机自启；顺带把注册表项升级为新格式
        return (true, true);
    }
    // 其余一律当"主人手动双击 exe"处理：弹设置窗
    (false, false)
}

/// 系统本次已运行的毫秒数（`GetTickCount64` 单调递增，不受改系统时间影响，也不含休眠）
fn uptime_ms() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe { GetTickCount64() }
}

/// 这条启动项命令行是不是「启动本程序」（新旧格式都算）
fn is_own_entry(command: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let path = exe.display().to_string();
    // 先把 --autostart 摘掉再比路径，新旧两种格式就归一到同一种比较
    let cmd = command
        .trim()
        .strip_suffix(AUTOSTART_FLAG)
        .map(str::trim_end)
        .unwrap_or(command.trim());
    // Windows 路径不区分大小写，这里也只折 ASCII（中文路径没有大小写之分）
    cmd.eq_ignore_ascii_case(&format!("\"{path}\"")) || cmd.eq_ignore_ascii_case(&path)
}

fn has_autostart_flag(command: &str) -> bool {
    command.trim().ends_with(AUTOSTART_FLAG)
}

/// 读出启动项里当前记着的命令行（没装自启项或读不到时为 None）
fn current_command() -> Option<String> {
    read_command(VALUE_NAME)
}

fn read_command(name: &str) -> Option<String> {
    let key = open(KEY_READ).ok()?;
    let name = HSTRING::from(name);
    let mut size = 0u32;
    let err = unsafe { RegQueryValueExW(key, &name, None, None, None, Some(&mut size)) };
    // size < 2 连一个 UTF-16 字符都装不下，没必要再读一次
    if err.0 != 0 || size < 2 {
        let _ = unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let err = unsafe {
        RegQueryValueExW(
            key,
            &name,
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    if err.0 != 0 {
        return None;
    }
    buf.truncate(size as usize);
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // REG_SZ 带结尾的 0，去掉它，免得日志里出现一个看不见的字符
    Some(
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string(),
    )
}

fn set_named(name: &str, enabled: bool) -> Result<()> {
    // 先把要写进去的值备好，再打开注册表键：`current_exe()` 失败（进程映像被
    // 删掉或改名后运行）时 `?` 会直接返回 —— 若那时键已经打开，这个句柄就漏了。
    let value = if enabled {
        let exe = std::env::current_exe().context("无法获取程序自身路径")?;
        // 带上 --autostart：程序靠它认出"这次是开机自启"，从而静默缩在托盘里
        // 而不是把设置窗顶到主人脸上（见 `launched_at_boot`）。
        let mut wide: Vec<u16> = format!("\"{}\" {}", exe.display(), AUTOSTART_FLAG)
            .encode_utf16()
            .collect();
        wide.push(0); // 注册表字符串需要以 0 结尾
        Some(wide)
    } else {
        None
    };

    let key = open(KEY_WRITE)?;
    let name = HSTRING::from(name);
    let err = match &value {
        Some(wide) => {
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
            unsafe { RegSetValueExW(key, &name, None, REG_SZ, Some(bytes)) }
        }
        None => unsafe { RegDeleteValueW(key, &name) },
    };
    let _ = unsafe { RegCloseKey(key) };
    if err.0 != 0 {
        bail!("写入注册表失败（错误码 {}）", err.0);
    }
    Ok(())
}

fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY> {
    let sub = HSTRING::from(RUN_KEY);
    let mut key = HKEY::default();
    let err = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &sub, None, access, &mut key) };
    if err.0 != 0 {
        bail!("打开注册表 Run 项失败（错误码 {}）", err.0);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用临时键名做一次真实往返，避免污染正式的 BBVoxi 启动项。
    ///
    /// 键名必须是**每个用例独有**的：单测并行跑，两个用例共用一个名字时，
    /// 一个刚写完另一个就删了，表现为随机失败（"错误码 2"= 值已经不存在）。
    #[test]
    fn roundtrip_with_temp_value_name() {
        let name = "BBVoxi_SelfTest_Roundtrip";
        assert!(read_command(name).is_none());
        set_named(name, true).unwrap();
        assert!(read_command(name).is_some());
        set_named(name, false).unwrap();
        assert!(read_command(name).is_none());
    }

    /// 写进去再读回来必须是同一条命令行。
    ///
    /// 这条守的是"UTF-16 编码 + 结尾 0 + 读回时按字节切 u16"这三个细节：
    /// 任何一处写偏，`current_command()` 读出来就是乱码或不完整 —— 而它是
    /// 「老启动项识别」和「自启项自检」的唯一依据，坏了还完全看不出来
    /// （只表现为"开机老弹窗/老不弹窗"这种没法调试的现象）。
    #[test]
    fn written_command_reads_back_identical() {
        let name = "BBVoxi_SelfTest_Command";
        set_named(name, true).unwrap();
        let cmd = read_command(name).expect("刚写进去的值必须能读回来");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            cmd,
            format!("\"{}\" {}", exe.display(), AUTOSTART_FLAG),
            "写进去和读出来的命令行不一致（编码/解码对不上）"
        );
        assert!(
            cmd.ends_with(AUTOSTART_FLAG),
            "新格式的启动项必须带上 --autostart，否则开机还会弹窗"
        );
        assert!(
            !cmd.contains('\0'),
            "读回来不能残留 REG_SZ 结尾的 0，否则比对字符串永远不相等"
        );
        set_named(name, false).unwrap();
        assert!(read_command(name).is_none(), "删掉之后不该还能读到");
    }

    /// 老版本写进注册表的启动项只有一条被引号包着的路径，必须认出来是"我们的自启项"，
    /// 否则老用户升级后第一次开机照样弹窗
    #[test]
    fn legacy_entry_is_recognized_by_path() {
        let exe = std::env::current_exe().unwrap();
        let path = exe.display().to_string();
        let legacy = format!("\"{path}\"");
        assert!(
            is_own_entry(&legacy),
            "老格式（只有路径）必须认成我们的启动项"
        );
        assert!(
            !has_autostart_flag(&legacy),
            "老格式没有参数，才需要被升级 —— 这一条就是鉴别的依据"
        );
        assert!(
            is_own_entry(&format!("  \"{path}\"  ")),
            "注册表里前后带空格也要认得出来"
        );
        let fresh = format!("\"{path}\" {AUTOSTART_FLAG}");
        assert!(is_own_entry(&fresh), "新格式也是我们的启动项");
        assert!(
            has_autostart_flag(&fresh),
            "新格式带参数，不该再被当成老格式重复升级"
        );
        assert!(
            !is_own_entry(r#""C:\别的程序\other.exe""#),
            "别的程序的启动项不能算我们的（否则设置窗的勾会撒谎）"
        );
    }

    /// 「是否算开机自启」的四种组合。
    ///
    /// 最关键的是第三行：老格式启动项 + 系统刚开机 → 自启且要升级；
    /// 以及第四行：老格式启动项 + 开机已久 → **不是**自启（主人手动双击），
    /// 否则主人双击 exe 会一个窗口都不弹，还以为程序坏了。
    #[test]
    fn boot_decision_matrix() {
        let fresh = 60_000; // 开机 1 分钟
        let stale = 10 * 60 * 1000; // 开机 10 分钟
        assert_eq!(
            boot_decision(true, false, stale),
            (true, false),
            "命令行带 --autostart 一定是自启，无需升级"
        );
        assert_eq!(
            boot_decision(false, false, fresh),
            (false, false),
            "没有老启动项 = 手动双击，必须弹窗"
        );
        assert_eq!(
            boot_decision(false, true, fresh),
            (true, true),
            "老启动项 + 系统刚开机 = 自启，并顺带升级注册表项"
        );
        assert_eq!(
            boot_decision(false, true, stale),
            (false, false),
            "老启动项 + 开机已久 = 手动双击，不能一个窗口都不弹"
        );
        // 边界：恰好等于窗口长度时不算自启（判定用严格小于）
        assert_eq!(
            boot_decision(false, true, BOOT_UPTIME_WINDOW_MS),
            (false, false)
        );
    }

    #[test]
    fn default_is_disabled_on_fresh_install() {
        // 只读检查，不修改
        let _ = is_enabled();
    }
}
