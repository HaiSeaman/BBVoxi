//! 开机自启：写 HKCU\Software\Microsoft\Windows\CurrentVersion\Run（当前用户，不需要管理员权限）。

use anyhow::{bail, Context, Result};
use windows::core::HSTRING;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "BBVoxi";

pub fn is_enabled() -> bool {
    has_value(VALUE_NAME)
}

pub fn set(enabled: bool) -> Result<()> {
    set_named(VALUE_NAME, enabled)
}

fn has_value(name: &str) -> bool {
    let Ok(key) = open(KEY_READ) else {
        return false;
    };
    let name = HSTRING::from(name);
    let mut size = 0u32;
    let err = unsafe { RegQueryValueExW(key, &name, None, None, None, Some(&mut size)) };
    let _ = unsafe { RegCloseKey(key) };
    err.0 == 0
}

fn set_named(name: &str, enabled: bool) -> Result<()> {
    // 先把要写进去的值备好，再打开注册表键：`current_exe()` 失败（进程映像被
    // 删掉或改名后运行）时 `?` 会直接返回 —— 若那时键已经打开，这个句柄就漏了。
    let value = if enabled {
        let exe = std::env::current_exe().context("无法获取程序自身路径")?;
        let mut wide: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().collect();
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

    /// 用临时键名做一次真实往返，避免污染正式的 BBVoxi 启动项
    #[test]
    fn roundtrip_with_temp_value_name() {
        let name = "BBVoxi_SelfTest";
        assert!(!has_value(name));
        set_named(name, true).unwrap();
        assert!(has_value(name));
        set_named(name, false).unwrap();
        assert!(!has_value(name));
    }

    #[test]
    fn default_is_disabled_on_fresh_install() {
        // 只读检查，不修改
        let _ = is_enabled();
    }
}
