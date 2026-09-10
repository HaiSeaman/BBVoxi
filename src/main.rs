#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod asr;
mod audio;
mod autostart;
mod config;
mod hotkey;
mod injector;
mod log;
mod session;
mod typer;
mod ui;

use anyhow::Result;
use eframe::egui;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use config::Config;
use session::{Cmd, Shared};

fn main() -> Result<()> {
    install_panic_hook();
    if !single_instance_lock() {
        eprintln!("BBVoxi 已在运行（可在系统托盘中打开设置）。");
        return Ok(());
    }

    let (cfg, first_run) = config::load_or_default()?;
    // --settings：即使已配置也直接打开设置窗（方便手动查看/排障）
    let force_settings = std::env::args().any(|a| a == "--settings");
    let (win_w, win_h, win_pos) = window_geometry();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([win_w, win_h])
            .with_min_inner_size([460.0, 560.0])
            .with_position(win_pos)
            .with_visible(first_run || force_settings)
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "BBVoxi 语音输入法",
        options,
        Box::new(move |cc| {
            setup_cjk_font(&cc.egui_ctx);
            ui::setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new(cc, cfg, force_settings)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("启动界面失败：{e}"))
}

/// 任何 panic 都写进日志再交给默认处理器。
/// 之前点「测试连接」直接闪退就是因为 panic 没留下任何线索，只能靠猜。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".into());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "无消息".into());
        log::log(format!("【崩溃】{location} → {message}"));
        previous(info);
    }));
}

/// 窗口尺寸与位置：高度跟着屏幕走并强制居中，
/// 否则 Windows 会每次启动把窗口往右下挪 52px，几次之后底部按钮就跑到屏幕外了。
fn window_geometry() -> (f32, f32, [f32; 2]) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if sw > 0 && sh > 0 {
            let w = 500.0f32;
            // 预留标题栏与任务栏的空间
            let h = ((sh as f32) - 160.0).clamp(560.0, 900.0);
            let x = ((sw as f32) - w) / 2.0;
            let y = (((sh as f32) - h) / 2.0 - 30.0).max(0.0);
            return (w, h, [x, y]);
        }
    }
    (500.0, 780.0, [120.0, 80.0])
}

/// 单实例保护：命名互斥量已存在说明已有实例在运行。/// ponytail: 只做互斥，不做进程间唤醒（需要再加 WM_COPYDATA）
fn single_instance_lock() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    unsafe {
        match CreateMutexW(None, false, windows::core::w!("BBVoxi_SingleInstance")) {
            Ok(h) if !h.is_invalid() => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                Box::leak(Box::new(h)); // 进程存活期间保持持有
                !already
            }
            _ => true,
        }
    }
}

/// egui 默认字体不含中文字形，必须挂一个系统字体，否则中文全是豆腐块。
fn setup_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 2] = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ];
    let Some(data) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        log::log("未找到系统中文字体，界面中文可能显示为方块");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), egui::FontData::from_owned(data).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrayState {
    Idle,
    Recording,
    Error,
}

struct App {
    settings: ui::SettingsApp,
    shared: Arc<Shared>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    tray: Option<TrayIcon>,
    tray_failed: bool,
    tray_state: TrayState,
    /// 上一次写入托盘菜单的录音状态（None = 尚未设置过）
    tray_recording: Option<bool>,
    /// 上一次写入托盘的提示文字
    tray_tooltip: Option<String>,
    /// 启动时要求把设置窗置前（--settings）
    focus_once: bool,
    open_item: MenuItem,
    record_item: MenuItem,
    quit_item: MenuItem,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config, force_settings: bool) -> Self {
        log::log("BBVoxi 启动");
        let shared = Arc::new(Shared::new(cc.egui_ctx.clone()));
        let cfg_shared = Arc::new(Mutex::new(cfg.clone()));
        let cmd_tx = session::spawn(cfg_shared.clone(), shared.clone());
        let paused = Arc::new(AtomicBool::new(false));

        let hotkey = match hotkey::parse(&cfg.hotkey) {
            Ok(hk) => hk,
            Err(e) => {
                log::log(format!(
                    "快捷键配置「{}」无法解析（{e}），已回退默认 Ctrl+1",
                    cfg.hotkey
                ));
                hotkey::Hotkey::default()
            }
        };
        if let Err(e) = hotkey::spawn(hotkey, cmd_tx.clone(), paused.clone()) {
            log::log(format!("启动全局快捷键失败：{e:#}"));
        }

        let settings = ui::SettingsApp::new(
            cfg,
            cfg_shared.clone(),
            shared.clone(),
            cmd_tx.clone(),
            paused.clone(),
        );

        Self {
            settings,
            shared,
            cmd_tx,
            tray: None,
            tray_failed: false,
            tray_state: TrayState::Idle,
            tray_recording: None,
            tray_tooltip: None,
            focus_once: force_settings,
            open_item: MenuItem::new("打开设置", true, None),
            record_item: MenuItem::new("开始录音", true, None),
            quit_item: MenuItem::new("退出", true, None),
        }
    }

    fn build_tray(&self) -> Result<TrayIcon> {
        let menu = Menu::new();
        menu.append(&self.open_item)?;
        menu.append(&self.record_item)?;
        menu.append(&self.quit_item)?;
        Ok(TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("BBVoxi 语音输入法")
            .with_icon(icon_for(TrayState::Idle))
            .build()?)
    }

    fn show_settings(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// 托盘图标与菜单文案跟随录音状态
    fn sync_tray(&mut self) {
        let snap = self.shared.snapshot();
        let state = if snap.recording {
            TrayState::Recording
        } else if snap.error.is_some() {
            TrayState::Error
        } else {
            TrayState::Idle
        };
        if self.tray_state != state {
            if let Some(tray) = &self.tray {
                if let Err(e) = tray.set_icon(Some(icon_for(state))) {
                    log::log(format!("更新托盘图标失败：{e}"));
                }
            }
            self.tray_state = state;
        }
        // 菜单文案只在状态翻转时改一次，避免每帧都去动原生菜单
        if self.tray_recording != Some(snap.recording) {
            self.record_item.set_text(if snap.recording {
                "停止录音"
            } else {
                "开始录音"
            });
            self.tray_recording = Some(snap.recording);
        }
        // 没有悬浮条了，托盘提示文字就是唯一的状态反馈
        let tooltip = if snap.recording {
            "BBVoxi · 正在录音，结束请再点一次或用快捷键"
        } else if snap.error.is_some() {
            "BBVoxi · 上次识别出错（打开设置查看）"
        } else {
            "BBVoxi 语音输入法"
        };
        if self.tray_tooltip.as_deref() != Some(tooltip) {
            if let Some(tray) = &self.tray {
                let _ = tray.set_tooltip(Some(tooltip));
            }
            self.tray_tooltip = Some(tooltip.to_string());
        }
    }
}

impl eframe::App for App {
    /// eframe 0.35：窗口隐藏时仍会调用 logic，托盘状态都在这里维护
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.tray.is_none() && !self.tray_failed {
            match self.build_tray() {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    log::log(format!("创建托盘图标失败：{e}"));
                    self.tray_failed = true;
                }
            }
        }
        self.sync_tray();

        // --settings 启动时把窗口拉到最前面（否则会被其他窗口挡住）
        if self.focus_once {
            self.focus_once = false;
            self.show_settings(ctx);
        }

        // 关闭按钮 = 隐藏到托盘，不退出进程
        if ctx.input(|i| i.viewport().close_requested()) {
            self.settings.cancel_capture();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.open_item.id() {
                self.show_settings(ctx);
            } else if event.id == self.record_item.id() {
                let recording = self.shared.snapshot().recording;
                log::log(if recording {
                    "托盘：停止录音"
                } else {
                    "托盘：开始录音"
                });
                let _ = self
                    .cmd_tx
                    .send(if recording { Cmd::Stop } else { Cmd::Start });
            } else if event.id == self.quit_item.id() {
                log::log("用户退出");
                std::process::exit(0);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = event {
                self.show_settings(ctx);
            }
        }

        // ponytail: 隐藏窗口时也要轮询托盘事件，10Hz 轮询代价可忽略
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 版面由设置界面自己安排（底部面板 + 中央面板），这里不要再套一层
        self.settings.ui(ui);
    }
}

fn icon_for(state: TrayState) -> Icon {
    let bytes = match state {
        TrayState::Idle => ICON_IDLE,
        TrayState::Recording => ICON_REC,
        TrayState::Error => ICON_ERR,
    };
    Icon::from_rgba(bytes.to_vec(), 32, 32).expect("图标资源应为 32x32 RGBA")
}

/// 设置窗口标题栏 / 任务栏 / Alt+Tab 用的图标（128x128 原始 RGBA）
fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: ICON_WINDOW.to_vec(),
        width: 128,
        height: 128,
    }
}

// 图标资源由 scripts/make_icons.py 从源图去背后导出（去掉了棋盘格背景，保留透明通道）
static ICON_IDLE: &[u8] = include_bytes!("../assets/rgba32_idle.bin");
static ICON_REC: &[u8] = include_bytes!("../assets/rgba32_rec.bin");
static ICON_ERR: &[u8] = include_bytes!("../assets/rgba32_err.bin");
static ICON_WINDOW: &[u8] = include_bytes!("../assets/rgba128_window.bin");
