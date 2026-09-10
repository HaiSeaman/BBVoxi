//! 把 assets/icon.ico 编译成 Windows 资源，链接进 exe —— 这样资源管理器、任务栏、
//! Alt+Tab 显示的才是自定义图标。直接调用 Windows SDK 的 rc.exe，不引入额外依赖；
//! 找不到 rc.exe 时只告警、不中断构建（图标退回系统默认）。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/bbvoxi.rc");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let Some(rc) = find_rc() else {
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，exe 将使用系统默认图标");
        return;
    };

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    let res = out.join("bbvoxi.res");
    // rc.exe 的工作目录决定 .rc 里相对路径的基准，所以切到 assets/
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("缺少项目目录"));
    let status = Command::new(&rc)
        .current_dir(manifest.join("assets"))
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg("bbvoxi.rc")
        .status();

    match status {
        Ok(s) if s.success() && res.exists() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {s}，exe 将使用系统默认图标"),
        Err(e) => println!("cargo:warning=执行 rc.exe 失败：{e}"),
    }
}

/// 在 Windows SDK 目录里找最新的 x64 rc.exe
fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let base = Path::new("C:\\Program Files (x86)\\Windows Kits\\10\\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|v| v.join("x64").join("rc.exe"))
        .find(|p| p.is_file())
}
