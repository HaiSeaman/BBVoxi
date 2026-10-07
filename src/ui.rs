//! 设置窗口。
//!
//! 设计基调取自应用图标本身（靛蓝→青绿的渐变），克制使用一个强调色：
//! 顶部品牌栏 + 三页签分区 + 卡片行布局 + 底部固定操作条；所有颜色来自
//! `Palette`，支持「跟随系统 / 浅色 / 深色」三选一（见 `ThemeMode`）。

use crate::audio;
use crate::autostart;
use crate::config::{
    Config, Provider, ThemeMode, LANGUAGES, TENCENT_ENGINES, TENCENT_URL,
};
use crate::hotkey::{self, Hotkey};
use crate::session::{Cmd, Shared, Snapshot};
use crate::update::{self, Release};
use eframe::egui;
use egui::{Color32, CornerRadius, Margin, RichText, Stroke};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// 应用图标（128×128 RGBA），用作窗口内的品牌标识
static ICON_WINDOW: &[u8] = include_bytes!("../assets/rgba128_window.bin");

/// 改键捕捉与界面心跳的容忍度：`logic()` 100ms 一帧，界面可见时 `ui()` 至多
/// 100ms 就被绘制一次，这里留 4 倍余量。超时即认定界面已不在（见
/// `SettingsApp::reap_capture_if_ui_gone`）。
const CAPTURE_IDLE_LIMIT: Duration = Duration::from_millis(400);

// —— 设计令牌 ——
struct Palette {
    bg: Color32,
    card: Color32,
    field: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    on_accent: Color32,
    ok: Color32,
    warn: Color32,
    danger: Color32,
}

impl Palette {
    fn of(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::from_rgb(0x10, 0x12, 0x18),
                card: Color32::from_rgb(0x18, 0x1B, 0x23),
                field: Color32::from_rgb(0x20, 0x24, 0x2E),
                border: Color32::from_rgb(0x2A, 0x30, 0x3C),
                text: Color32::from_rgb(0xEC, 0xEE, 0xF3),
                muted: Color32::from_rgb(0x98, 0xA2, 0xB3),
                accent: Color32::from_rgb(0x7C, 0x86, 0xFF),
                on_accent: Color32::from_rgb(0x0B, 0x0D, 0x12),
                ok: Color32::from_rgb(0x3D, 0xD6, 0x8C),
                warn: Color32::from_rgb(0xF5, 0xB5, 0x44),
                danger: Color32::from_rgb(0xF9, 0x70, 0x66),
            }
        } else {
            Self {
                bg: Color32::from_rgb(0xF5, 0xF6, 0xF8),
                card: Color32::WHITE,
                field: Color32::from_rgb(0xF7, 0xF8, 0xFA),
                border: Color32::from_rgb(0xE2, 0xE5, 0xEB),
                text: Color32::from_rgb(0x16, 0x18, 0x1D),
                muted: Color32::from_rgb(0x66, 0x70, 0x85),
                accent: Color32::from_rgb(0x54, 0x57, 0xE5),
                on_accent: Color32::WHITE,
                // 浅色主题的三个状态色**必须比深色主题深**：它们要印在白卡片上，
                // 还要印在"自己 × 12% 透明度"的胶囊底上。旧值（#12A150 / #C77A00）
                // 实测只有 3.37 / 3.38:1，胶囊上更是 2.88 —— 12px 的字基本读不出来
                // （报告里点名的两条）。这三个值是按 WCAG 公式挑的，实测
                // 白卡片上 6.2+、胶囊上 5.0+。
                ok: Color32::from_rgb(0x0A, 0x70, 0x38),
                warn: Color32::from_rgb(0x8A, 0x52, 0x00),
                danger: Color32::from_rgb(0xB7, 0x22, 0x0F),
            }
        }
    }
}

// —— 对比度：WCAG 2.x 相对亮度公式 ——
//
// 为什么值得专门摆在这里：以前那条对比度单测只比较 R 通道差值，实测出来的
// 2.88、3.37 一个都抓不到（报告里点名的"假安全网"）。界面配色是不是能看清，
// 只能靠这条公式说了算。

/// sRGB 分量 → 线性值（WCAG 2.x 的定义，不是简单的平方）
fn linearize(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 相对亮度
fn relative_luminance(color: Color32) -> f64 {
    let [r, g, b, _] = color.to_array();
    0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b)
}

/// WCAG 对比度（1.0 ~ 21.0）。正文门槛 4.5:1，大字（18pt 粗体或 24px）3.0:1。
#[allow(dead_code)] // 只有测试用得到它，但这就是它存在的意义
pub fn contrast_ratio(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 半透明色叠在底色上之后的实际颜色。
///
/// egui 画半透明色就是"前景 × 透明度 + 底色 × (1 − 透明度)"，
/// 所以"胶囊上那行字到底能不能看清"必须按叠完之后的底色来算。
fn blend_over(bg: Color32, fg: Color32, alpha: f32) -> Color32 {
    let [fr, fg_, fb, _] = fg.to_array();
    let [br, bg_, bb, _] = bg.to_array();
    let mix = |f: u8, b: u8| (f as f32 * alpha + b as f32 * (1.0 - alpha)).round() as u8;
    Color32::from_rgb(mix(fr, br), mix(fg_, bg_), mix(fb, bb))
}

/// 状态胶囊 / 选中项底色：状态色 × 透明度叠在卡片上（透明度见 `PILL_ALPHA`）
fn tint_over(card: Color32, color: Color32) -> Color32 {
    blend_over(card, color, PILL_ALPHA)
}

/// 状态胶囊 / 选中项底色的透明度。
///
/// **不能随手加大**：底色越深，上面那行 12px 的字对比度越低。报告建议"14% → 25%"
/// 是想让胶囊更显眼，但按 WCAG 公式实测，25% 会把字压到 4.5:1 以下 ——
/// 所以这里反而收到 0.12，改用"加深状态色"把可读性补回来（浅色主题的
/// ok/warn/danger 就是为此调深的，实测字与底 5.2:1 以上）。
const PILL_ALPHA: f32 = 0.12;

/// 被禁用控件的整体透明度（egui 默认 0.5）。
///
/// egui 的禁用是"整个控件乘一个透明度"画的：0.5 时浅色主题下正文会被压到 3.37:1，
/// 报告实测"自动添加标点""口语顺滑"被禁用时几乎看不见（1.99）。0.65 既还看得出
/// "这是灰的"，又保证字仍然读得清（实测 5.5:1）。
const DISABLED_ALPHA: f32 = 0.65;

/// 全局控件风格（圆角、内边距、交互高度、字号档位）
pub fn setup_style(ctx: &egui::Context) {
    // egui 0.35：没有 set_style，按主题统一改
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        for w in [
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        // 字号收敛到语义档位（旧代码有 47 处硬编码的 11/11.5/12/13/13.5/14/15/21，
        // 想整体调一次字号得逐处改，还必然改漏几处）。四档 + 等宽：
        // Heading 品牌名 / Body 正文与选项 / Button 按钮 / Small 辅助说明。
        style.text_styles = [
            (egui::TextStyle::Heading, egui::FontId::proportional(19.0)),
            (egui::TextStyle::Body, egui::FontId::proportional(13.0)),
            (egui::TextStyle::Button, egui::FontId::proportional(13.0)),
            (egui::TextStyle::Small, egui::FontId::proportional(11.5)),
            (egui::TextStyle::Monospace, egui::FontId::monospace(15.0)),
        ]
        .into();
        // 状态切换做一点点过渡（egui 自带的动画开关）：卡片出现、按钮变色不再硬切。
        // 0.12 秒：够看出"过渡"，又不会让点击显得迟钝。
        style.animation_time = 0.12;
    });
}

/// 每帧把主题色灌进控件视觉（支持系统深浅色切换）
fn apply_widget_style(ui: &mut egui::Ui, p: &Palette) {
    let v = ui.visuals_mut();
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.field;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.disabled_alpha = DISABLED_ALPHA;

    // 没显式取色的文字（复选框标签、键帽、说明……）走这一项。egui 默认是 gray(140)：
    // 深色主题下它比 muted 还淡（正文层级颠倒），浅色主题下被禁用时几乎看不见。
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    // 三个交互态的**文字色（也是复选框勾的颜色）必须一致**：egui 直接按状态取色，
    // 以前 inactive = muted、hovered/active = text，于是鼠标一划过去文字就从灰变黑
    // （复选框尤其明显）。悬停反馈交给 bg_stroke / bg_fill 表达，不靠文字颜色跳变。
    v.widgets.inactive.weak_bg_fill = p.field;
    v.widgets.inactive.bg_fill = p.field;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.weak_bg_fill = p.field;
    v.widgets.hovered.bg_fill = p.field;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.55));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.weak_bg_fill = p.field;
    v.widgets.active.bg_fill = p.field;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
}

/// 一条状态消息（底部操作条上方那行小字）。
///
/// 带时间戳是为了让它**到点自己退场**：旧版只写不清，一条「已保存」会永远挂在
/// 界面上，还把卡片越顶越高（报告第 3 条）。新消息直接顶掉旧消息 —— 不做队列，
/// 主人只关心"最后发生了什么"。
#[derive(Clone)]
struct Status {
    ok: bool,
    text: String,
    shown_at: Instant,
}

/// 状态消息显示多久：够看清一句话，又不至于永远占着那一行
const STATUS_TTL: Duration = Duration::from_secs(10);

/// 「检查更新」按钮点下去之后走到哪一步了。
///
/// 为什么要有这个状态机：查版本和下载都要联网，**绝不能卡在界面线程上**，
/// 所以放在后台线程里跑，界面按这个状态显示"正在检查 / 正在下载 42% / 装好了"。
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateState {
    /// 什么都没做
    Idle,
    /// 正在问 GitHub 有没有新版本
    Checking,
    /// 查到新版本，**等主人点确认**才下载（主人选的"先问一句再装"）
    Found(Box<Release>),
    /// 正在下载
    Downloading { version: String, done: u64, total: u64 },
    /// 已经装好，重启才生效
    Installed { version: String },
    /// 正在起新版本（起完由 `main` 收尾退出）
    Restarting,
    /// 哪一步失败了（大白话原因）
    Failed(String),
}

/// 后台线程往界面回的消息
enum UpdateMsg {
    Found(Box<Release>),
    UpToDate(String),
    Progress(u64, u64),
    Installed(String),
    /// 新版本进程已经起来了，可以退出本进程了
    RestartOk,
    Failed(String),
}

/// 这条状态消息还该显示吗（纯函数，边界钉在单测里）
fn status_is_fresh(shown_at: Instant, now: Instant) -> bool {
    now.duration_since(shown_at) < STATUS_TTL
}

/// 状态胶囊该显示什么（纯函数，便于单测钉住"什么才算需要提醒"）。
///
/// 关键：**"照例留了一份到剪贴板"不是异常**。「识别结果总留一份到剪贴板」默认开着，
/// 每次识别成功都会留 —— 那不是提醒，不该常亮警告色（托盘也不会因此报警）。
/// 只有 `notice`（这次没能自动输入、结果改走剪贴板）才是"请看一眼"。
fn pill_state(
    snap: &Snapshot,
    capturing: bool,
    credentials_ready: bool,
    p: &Palette,
) -> (Color32, &'static str) {
    if snap.recording {
        (p.danger, "录音中")
    } else if snap.error.is_some() {
        (p.danger, "出错了")
    } else if snap.notice.is_some() {
        (p.warn, "有提示")
    } else if capturing {
        // 与「录音中」一字之差完全分不清，改成明确指向快捷键
        (p.warn, "改键中")
    } else if !credentials_ready {
        (p.warn, "未配置")
    } else {
        (p.ok, "就绪")
    }
}

/// 设置窗口的三个页签。
///
/// 为什么是三个、为什么用顶部页签而不是侧边栏：设置的分组只有 3 个，
/// 窄窗口（580px）里侧边栏要吃掉三分之一宽度；顶部页签让每个分组
/// 独占一屏，改一个开关不用再从一堆 API Key 里翻山越岭。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tab {
    /// 服务商与凭据（一次性配置，配好就不动）
    Service,
    /// 识别行为开关（日常最常调的一页）
    Options,
    /// 快捷键 / 开机自启 / 界面主题
    General,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Service, Tab::Options, Tab::General];
    fn label(self) -> &'static str {
        match self {
            Tab::Service => "语音服务",
            Tab::Options => "识别选项",
            Tab::General => "通用",
        }
    }
}

/// 把配置里的主题选择灌进 egui（跟随系统 / 浅色 / 深色）。
///
/// `Palette` 每帧按 `visuals().dark_mode` 取色，所以这一句生效后
/// 下一帧整套配色（含对比度校验过的所有文字色）自动跟上。
pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    let pref = match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    };
    ctx.set_theme(pref);
}

pub struct SettingsApp {
    edit: Config,
    /// 上一次保存（或刚加载）时的那份配置，见 `has_unsaved_changes`
    saved: Config,
    shared_cfg: Arc<Mutex<Config>>,
    shared: Arc<Shared>,
    cmd_tx: UnboundedSender<Cmd>,
    paused: Arc<AtomicBool>,
    show_keys: bool,
    status: Option<Status>,
    /// 关窗确认对话框开着吗（点 ✕ 时若有没保存的改动就打开）
    confirm_close: bool,
    /// 关窗确认里选了"关"：窗口显隐由 `main` 统一执行（只有一处管，状态才不会走偏）
    hide_requested: bool,
    autostart: bool,
    applied_autostart: bool,
    capturing: bool,
    hotkey_error: Option<String>,
    /// 当前停在哪个页签（默认语音服务：首次运行最要紧的事就是填凭据）
    tab: Tab,
    /// 捕捉过程中记下的「当前凑齐的修饰键」。
    ///
    /// 为什么需要它：像 Ctrl+Win 这种**纯修饰键组合**没有主键可听，而 egui 只为
    /// 非修饰键产生按键事件（Ctrl / Win 自己按下去，egui 那边什么都没有）。
    /// 所以只能一边读钩子的修饰键状态一边等 —— 等主人把手指全部松开，
    /// 那一刻记下的才是完整组合（见 `capture_step`）。
    pending_mods: Option<hotkey::Mods>,
    /// 上一次 `ui()` 被绘制的时刻。改键捕捉的生命周期靠它兜底，见
    /// `reap_capture_if_ui_gone`。
    painted_at: Instant,
    /// 通用页「麦克风」下拉栏的候选（本机所有能录音的设备）。
    ///
    /// 每次打开设置窗时由 `main::show_settings` 重新列一遍（拔插设备后靠「刷新」按钮），
    /// 不是每帧都去枚举 —— 枚举要问系统要数据，而下拉栏只有「通用」页那一处用得到。
    mics: Vec<audio::MicDevice>,
    /// 列麦克风失败的原因（枚举挂了才有值，界面上如实显示出来）
    mics_error: Option<String>,
    /// 自动更新走到哪一步了（见 `UpdateState`）
    update: UpdateState,
    /// 后台线程回消息的通道（查版本/下载都在后台跑，界面不卡）
    update_rx: Option<std::sync::mpsc::Receiver<UpdateMsg>>,
    /// 主人点了「立刻重启」：交给 `main` 统一执行退出（先收托盘再退，见 main 的退出路径）
    restart_requested: bool,
}

impl SettingsApp {
    pub fn new(
        cfg: Config,
        shared_cfg: Arc<Mutex<Config>>,
        shared: Arc<Shared>,
        cmd_tx: UnboundedSender<Cmd>,
        paused: Arc<AtomicBool>,
    ) -> Self {
        let autostart = autostart::is_enabled();
        let saved = cfg.clone();
        let mut app = Self {
            edit: cfg,
            saved,
            shared_cfg,
            shared,
            cmd_tx,
            paused,
            show_keys: false,
            status: None,
            confirm_close: false,
            hide_requested: false,
            autostart,
            applied_autostart: autostart,
            capturing: false,
            hotkey_error: None,
            tab: Tab::Service,
            pending_mods: None,
            painted_at: Instant::now(),
            mics: Vec::new(),
            mics_error: None,
            update: UpdateState::Idle,
            update_rx: None,
            restart_requested: false,
        };
        // 一开始就把麦克风列出来：主人打开「通用」页时下拉栏里就该有东西可选，
        // 而不是要他自己先想起来点一下「刷新」。
        app.refresh_mics();
        app
    }

    /// 重新列一遍本机的麦克风（`new` 里一次，之后每次打开设置窗 `main::show_settings`
    /// 会再列一次，插拔设备后还有通用页的「刷新」按钮）。
    pub fn refresh_mics(&mut self) {
        match audio::list_input_devices() {
            Ok(list) => {
                self.mics = list;
                self.mics_error = None;
            }
            Err(e) => {
                // 列表读不出来也不能让设置窗出问题：留下原因显示出来，
                // 下拉栏照旧能用（「系统默认」这条路永远在）
                self.mics.clear();
                self.mics_error = Some(format!("{e:#}"));
            }
        }
    }

    /// 记一条状态消息（工具条上方那行小字，10 秒后自己退场）
    fn set_status(&mut self, ok: bool, text: impl Into<String>) {
        self.status = Some(Status {
            ok,
            text: text.into(),
            shown_at: Instant::now(),
        });
    }

    // —— 自动更新 ——
    //
    // 三层：查版本 → 请主人确认 → 下载+校验+替换 → 提示重启。
    // 查和下载都在**后台线程**里跑（联网绝不能卡界面），靠 `UpdateMsg` 回话。

    /// 点「检查更新」：后台去问 GitHub 有没有新版本
    fn start_update_check(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.update = UpdateState::Checking;
        // `cargo` 在编译时把版本号烧进二进制：这正是界面上显示的那个 v1.4.0
        let current = env!("CARGO_PKG_VERSION").to_string();
        std::thread::Builder::new()
            .name("bbvoxi-update-check".into())
            .spawn(move || {
                let msg = match update::latest() {
                    Ok(rel) if update::is_newer(&current, &rel.version) => {
                        UpdateMsg::Found(Box::new(rel))
                    }
                    // 线上不比本机新：如实说"已是最新"，绝不提示"发现新版本"
                    Ok(_) => UpdateMsg::UpToDate(current),
                    Err(e) => UpdateMsg::Failed(format!("{e:#}")),
                };
                let _ = tx.send(msg);
            })
            .ok();
        crate::log::log("开始检查更新");
    }

    /// 主人在确认窗里点了「现在更新」：后台下载 → 校验指纹 → 替换自身
    fn start_update_download(&mut self, release: Release) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let version = release.version.clone();
        self.update = UpdateState::Downloading {
            version: version.clone(),
            done: 0,
            total: 0,
        };
        std::thread::Builder::new()
            .name("bbvoxi-update-download".into())
            .spawn(move || {
                let progress = tx.clone();
                let result = update::download_and_verify(&release, |done, total| {
                    // 通道发不出去（界面没了）就当没事：下载本身继续，别因此中断
                    let _ = progress.send(UpdateMsg::Progress(done, total));
                })
                .and_then(|path| update::install(&path));
                let msg = match result {
                    Ok(()) => UpdateMsg::Installed(version),
                    Err(e) => UpdateMsg::Failed(format!("{e:#}")),
                };
                let _ = tx.send(msg);
            })
            .ok();
        crate::log::log("开始下载更新");
    }

    /// 每帧把后台线程的消息收干净（无消息就是空转，几乎不花时间）。
    ///
    /// 通道**断开**（后台线程 panic 或被杀）也要处理：那时 `try_recv` 会返回
    /// `Disconnected`，一条消息都收不到。不处理的话状态就永远停在"检查中/下载中"，
    /// 按钮一直灰着 —— 主人以为程序卡死，只能重启。这里当成一次失败收场。
    fn poll_update(&mut self) {
        let mut inbox = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.update_rx {
            loop {
                match rx.try_recv() {
                    Ok(msg) => inbox.push(msg),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for msg in inbox {
            self.apply_update_msg(msg);
        }
        if disconnected {
            self.update_rx = None;
            // 已经收到最终消息（状态不再是"进行中"）时，断开是正常的收场
            if matches!(
                self.update,
                UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Restarting
            ) {
                self.apply_update_msg(UpdateMsg::Failed(
                    "更新线程意外退出了（详情见日志），请再点一次「检查更新」".into(),
                ));
            }
        }
    }

    /// 处理一条后台消息（抽成方法：状态怎么变、界面说什么，都在这一个地方）
    fn apply_update_msg(&mut self, msg: UpdateMsg) {
        match msg {
            UpdateMsg::Found(rel) => {
                self.update = UpdateState::Found(rel);
            }
            UpdateMsg::UpToDate(version) => {
                self.update = UpdateState::Idle;
                self.update_rx = None;
                self.set_status(true, format!("已是最新版本（v{version}）"));
            }
            UpdateMsg::Progress(done, total) => {
                if let UpdateState::Downloading { version, .. } = &self.update {
                    self.update = UpdateState::Downloading {
                        version: version.clone(),
                        done,
                        total,
                    };
                }
            }
            UpdateMsg::Installed(version) => {
                self.update = UpdateState::Installed {
                    version: version.clone(),
                };
                self.update_rx = None;
                crate::log::log(format!("更新已装好（v{version}），等待重启"));
            }
            // 新版本进程已经起来了：`main` 收到这个请求后会先收托盘再退出
            UpdateMsg::RestartOk => {
                self.restart_requested = true;
            }
            UpdateMsg::Failed(why) => {
                crate::log::log(format!("更新失败：{why}"));
                self.update = UpdateState::Failed(why.clone());
                self.update_rx = None;
                // 状态消息里带上"更新失败"四个字：主人可能已经忘了刚才点过什么，
                // 只看到底部一行红字，得让他一眼知道这是更新的事
                self.set_status(false, format!("更新失败：{why}"));
            }
        }
    }

    /// 取走"该重启到新版本了"这个请求（取走即清零，由 `main` 执行退出）
    pub fn take_restart_request(&mut self) -> bool {
        std::mem::take(&mut self.restart_requested)
    }

    /// 主人点了「立刻重启」：先起新进程，再让 `main` 收尾退出。
    ///
    /// 为什么退出不在这里做：托盘图标要**先析构再退**（`process::exit` 不跑 Drop，
    /// 图标会残留在托盘里），那套收尾只有 `main` 有。所以这里只负责"起新进程"，
    /// 起不来就把原因显示出来，绝不装作重启了。
    ///
    /// 起进程这件事也放**后台线程**：`update::restart()` 里要等一小会儿确认新进程
    /// 没当场死掉，在界面线程上做就是半秒冻结。
    fn restart_into_new_version(&mut self) {
        // 正在录音时**不许重启**：`process::exit` 会把这次会话连同主人正在说的
        // 那句话一起丢掉（推流断了、结果也没了）。等他说完再点一次就行 ——
        // 用一句人话说清楚，而不是让按钮点了没反应。
        if self.shared.snapshot().recording {
            self.set_status(false, "正在录音：等这句话说完再重启，不然这次就白说了");
            return;
        }
        // 有改动还没保存时**也不许重启**：`main` 收到重启请求是直接
        // `process::exit(0)`（托盘图标必须先析构，见那边的说明），不会走任何保存
        // 路径 —— 底栏那颗"未保存"的小圆点还亮着就重启，主人刚改的东西就没了，
        // 而关窗路径明明会拦他一下，这条路却一声不吭。
        // 这里**不替主人自动保存**：他可能正打到一半的 API Key，写进文件反而是添乱。
        // 提示他先点保存，弹窗保持不动，保存完再点一次「立刻重启」即可。
        if self.has_unsaved_changes() {
            self.set_status(
                false,
                "有改动还没保存：先点「保存」，再点「立刻重启」，否则改动会丢",
            );
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.update = UpdateState::Restarting;
        std::thread::Builder::new()
            .name("bbvoxi-update-restart".into())
            .spawn(move || {
                let msg = match update::restart() {
                    Ok(()) => UpdateMsg::RestartOk,
                    Err(e) => UpdateMsg::Failed(format!("{e:#}")),
                };
                let _ = tx.send(msg);
            })
            .ok();
    }

    /// 到点的状态消息自己退场（见 `Status` 的说明）
    fn expire_status(&mut self, now: Instant) {
        if let Some(s) = &self.status {
            if !status_is_fresh(s.shown_at, now) {
                self.status = None;
            }
        }
    }

    /// 有没有改了还没保存的内容（`main` 在收到关闭请求时问这一句）。
    ///
    /// 判断错了两边都疼：判漏了，改动被无声丢掉；判多了，每次都拦着不让关 ——
    /// 所以这里比的是**整份配置**（`Config` 的相等性），不挑字段。
    pub fn has_unsaved_changes(&self) -> bool {
        self.edit != self.saved
    }

    /// 有没保存的改动时，关闭请求改为"先问一句"
    pub fn ask_before_close(&mut self) {
        self.confirm_close = true;
    }

    /// 取走"该把窗口收起来了"这个请求（取走即清零，由 `main` 执行隐藏）
    pub fn take_hide_request(&mut self) -> bool {
        std::mem::take(&mut self.hide_requested)
    }

    /// 窗口隐藏到托盘时取消快捷键捕捉，避免钩子一直处于暂停状态
    pub fn cancel_capture(&mut self) {
        if self.capturing {
            self.end_capture();
        }
    }

    /// 界面已经不在（`ui()` 迟迟没被绘制）就结束改键捕捉，返回是否结束了。
    ///
    /// 为什么需要这条兜底：捕捉期间钩子是**暂停**的（`paused=true`），按键原样
    /// 交给窗口。而主窗口一旦收起，eframe 就不再调用 `ui()`（`logic()` 照常
    /// 10Hz 跑），于是捕捉既结束不了、`paused` 也永远留在 true —— 全局快捷键
    /// **彻底失效**（按什么键都没反应），而且没有任何提示。
    ///
    /// 不逐个去补"隐藏窗口"的调用点：已知的隐藏路径就有两条（关闭按钮、
    /// 托盘触发录音时的 `hide_settings`），将来还会有第三条。这里判的是
    /// 真正的不变量 —— *捕捉只能在设置界面正在被绘制时存在*。
    pub fn reap_capture_if_ui_gone(&mut self) -> bool {
        if !capture_is_stale(self.capturing, self.painted_at.elapsed()) {
            return false;
        }
        self.end_capture();
        true
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        // 心跳：`logic()` 靠它判断"界面还在不在"，见 `reap_capture_if_ui_gone`
        self.painted_at = Instant::now();
        self.expire_status(Instant::now());
        // 先把后台线程的消息收掉：更新进度／结果都在这一帧反映出来
        self.poll_update();
        self.handle_capture(ui.ctx());
        let p = Palette::of(ui.visuals().dark_mode);
        apply_widget_style(ui, &p);

        // 关窗确认画在最上层（有没保存的改动时才会出现）
        self.close_confirm(ui.ctx(), &p);
        // 更新的两个小窗（"发现新版本"/"装好了，重启吗"）也在最上层
        self.update_dialogs(ui.ctx(), &p);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                ui.add_space(2.0);
                self.header(ui, &p);
                ui.add_space(8.0);
                // 页签常驻滚动区外：切页永远一步可达
                self.tabs_bar(ui, &p);
                ui.add_space(8.0);
                // 滚动区限高：可用高度减去页脚，页脚（反馈条 + 按钮行）才能
                // 顺排其后、钉在窗口底。`auto_shrink(false)` 只会让滚动区吃满
                // "给定"的空间，若不限高它会把页脚整块挤到面板外裁掉。
                // FOOTER_H 必须 ≥ 页脚真实高度（分隔线区 + 反馈条 + 按钮行），
                // 现在按钮行不再有动态的状态行，高度是恒定的。
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(ui.available_height() - FOOTER_H)
                    // 滚动条平时不占位置（滚轮照样能滚）：界面更干净，
                    // 而且省下的一条竖向空间正好给内容用
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.add_space(2.0);
                        // 控件 id 按页隔离：切页时各页输入框的焦点/光标状态互不串
                        ui.push_id(self.tab, |ui| match self.tab {
                            Tab::Service => self.service_page(ui, &p),
                            Tab::Options => self.options_page(ui, &p),
                            Tab::General => self.general_page(ui, &p),
                        });
                        ui.add_space(6.0);
                    });
                // —— 页脚：「最近识别」反馈条 + 按钮行，钉在窗口底 ——
                //
                // 不用 `egui::Panel::bottom`：egui 0.35 的 Panel 第一帧用
                // "interact_size + 边距"当猜测高度来布局，内容比它高时溢出部分
                // 直接被裁掉（实测按钮整行画到窗口外，保存/测试按钮全看不见，
                // 要等第二帧面板记忆了真实高度才恢复）。ScrollArea 竖向
                // auto_shrink=false 会吃掉上方全部剩余空间，页脚顺排其后，
                // 天然贴底，而且单帧布局就是对的。
                ui.add_space(4.0);
                // 分隔线跨满窗口宽：普通控件画不到面板边距外，借 clip_rect 撑
                let clip = ui.clip_rect();
                ui.painter().hline(
                    clip.left()..=clip.right(),
                    ui.cursor().top() + 2.0,
                    Stroke::new(1.0, p.border),
                );
                ui.add_space(8.0);
                self.result_strip(ui, &p);
                ui.add_space(6.0);
                self.actions_bar(ui, &p);
            });
    }

    // —— 顶部品牌栏 ——

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let hotkey_text = hotkey::parse(&self.edit.hotkey)
            .map(|h| h.display())
            .unwrap_or_else(|_| self.edit.hotkey.clone());

        ui.horizontal(|ui| {
            let texture = icon_texture(ui.ctx());
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                texture.id(),
                egui::vec2(36.0, 36.0),
            )));
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(heading("BBVoxi").strong().color(p.text));
                // 截断而不是换行：快捷键很长时（四个修饰键 + 主键就有 40 多个字）
                // 一换行会把品牌栏撑高、挤走下面的内容。截断宽度由布局自己兜住 ——
                // 实测长快捷键下胶囊仍稳稳在窗口内（见 `layout` 模块的用例）
                ui.add(
                    egui::Label::new(
                        small(format!("按住 {hotkey_text} 说话，松开自动输入")).color(p.muted),
                    )
                    .truncate(),
                );
            });
            self.status_pill(ui, p);
        });
    }

    /// 右上角的状态胶囊（底色 = 状态色 × `PILL_ALPHA` 叠在页面底色上）
    fn status_pill(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        let credentials_ready = self.edit.credentials_ready();
        let (color, text) = pill_state(&snap, self.capturing, credentials_ready, p);

        // 自己算宽度推到行尾（见 `push_to_end`：egui 的右对齐布局会画到容器外）。
        // 胶囊实际宽 = 内边距 20 + 圆点 7 + 圆点与文字的间距（add_space 2 +
        // item_spacing 8）+ 文字宽 —— 间距必须按真实值算，算小了胶囊会把
        // 整个面板的 max_rect 撑宽（实测过）。
        let pill_w = text_width(ui, text, egui::TextStyle::Small)
            + 7.0
            + 2.0
            + ui.style().spacing.item_spacing.x
            + 20.0;
        push_to_end(ui, pill_w);
        egui::Frame::new()
            .fill(tint_over(p.bg, color))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(10, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, color);
                    ui.add_space(2.0);
                    ui.label(small(text).color(color).strong());
                });
            });
    }

    // —— 页签栏 ——

    /// 三个页签（选中样式与分段选择器一致：accent 胶囊底 + accent 字）
    fn tabs_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut tab = self.tab;
        segmented(ui, p, p.bg, &mut tab, &Tab::ALL, Tab::label, true);
        self.tab = tab;
    }

    // —— 三个页签的内容 ——

    /// 语音服务页：服务商选择 + 凭据 + 个人词典（一次性配置，配好就不动）。
    /// 词典放这页而不是「识别选项」：它是"教当前服务商认词"——千问/豆包生效、
    /// 腾讯要控制台预建，本来就跟着服务商走。
    fn service_page(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            labeled_row(ui, "服务商", p, |ui| {
                provider_selector(ui, p, &mut self.edit.provider);
            });

            // 凭据跟着服务商变。`push_id` 把三套字段的控件 id 分开：
            // 否则切服务商时 egui 会把上一套的焦点/光标状态带到下一套字段上。
            ui.push_id(self.edit.provider.label(), |ui| self.credentials(ui, p));

            sep(ui, p);
            self.hotwords_section(ui, p);
        });
    }

    /// 识别选项页：全部开关，一行一条，说明直接写在行里（不用悬停气泡）
    fn options_page(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            setting_row(
                ui,
                p,
                "实时输入",
                "边说话边打字，识别被修正时自动回退重打；关闭则只在松手后一次性输入",
                &mut self.edit.options.live_typing,
                true,
            );
            sep(ui, p);
            setting_row(
                ui,
                p,
                "剪贴板粘贴兜底",
                "输入被目标程序拒绝时改走粘贴；管理员窗口连粘贴也会被拦，那种不白试",
                &mut self.edit.options.clipboard_fallback,
                true,
            );
            sep(ui, p);
            setting_row(
                ui,
                p,
                "结果总留一份到剪贴板",
                "随时可 Ctrl+V 粘贴；代价是原来复制的内容会被顶掉（推荐开启）",
                &mut self.edit.options.keep_on_clipboard,
                true,
            );
            sep(ui, p);

            // 标点：腾讯引擎由服务端决定，不支持时禁用并把原因写进说明 ——
            // 不能给主人一个看起来能拨、实际不接线的开关。
            // （口语顺滑三家都已接通：千问 3.1 disfluency_removal_enabled、
            // 豆包 enable_ddc、腾讯 filter_modal。）
            let punc_ok = !matches!(self.edit.provider, Provider::Tencent);
            setting_row(
                ui,
                p,
                "自动添加标点",
                if punc_ok {
                    "识别结果自动补全逗号、句号等标点"
                } else {
                    "腾讯云引擎由服务端决定，暂不支持此选项"
                },
                &mut self.edit.options.auto_punctuation,
                punc_ok,
            );
            sep(ui, p);
            setting_row(
                ui,
                p,
                "口语顺滑",
                "过滤「嗯、啊」等语气词与重复表述",
                &mut self.edit.options.smooth,
                true,
            );
            sep(ui, p);
            self.language_row(ui, p);

            // 高精度是豆包专属：只在该服务商下出现，别的主人不需要看见它
            if self.edit.provider == Provider::Doubao {
                sep(ui, p);
                setting_row(
                    ui,
                    p,
                    "高精度模式",
                    "整句二次识别，更准一点，但会晚一两秒出结果",
                    &mut self.edit.doubao.high_accuracy,
                    true,
                );
            }
        });
    }

    /// 识别语言：单行（标签 + 下拉 + 说明气泡，与凭据区同款模式）。
    /// 三家落实方式不同，详细说明进气泡；选「自动」时行为与从前完全一致。
    fn language_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let tip = match self.edit.provider {
            // 千问模型自身多语种自动检测，选语言不产生任何参数
            Provider::Qwen => "千问模型自动检测语种（含中英混说），保持「自动」即可",
            Provider::Doubao => "选定后豆包按该语言识别。官方语言参数仅在整句模式下\
生效，选定后会自动切换（出字稍晚一两秒）",
            Provider::Tencent => "选定后腾讯使用对应语言引擎。法语 / 德语 / 俄语 / \
西班牙语没有实时引擎，识别时会提示换服务商或改回「自动」",
        };
        labeled_row(ui, "识别语言", p, |ui| {
            let selected = LANGUAGES
                .iter()
                .find(|(code, _)| *code == self.edit.options.language)
                .map(|(_, label)| *label)
                .unwrap_or("自动");
            egui::ComboBox::from_id_salt("language")
                .selected_text(body(selected))
                .width((ui.available_width() - INFO_W - 8.0).max(80.0))
                .show_ui(ui, |ui| {
                    for (code, label) in LANGUAGES {
                        ui.selectable_value(
                            &mut self.edit.options.language,
                            code.to_string(),
                            label,
                        );
                    }
                });
            info(ui, p, tip);
        });
    }

    /// 个人词典（热词）：一行一个词。千问走即时热词 + 上下文增强、
    /// 豆包走请求级热词；腾讯需控制台预建词表，这里如实说明。
    fn hotwords_section(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let desc = if self.edit.provider == Provider::Tencent {
            "腾讯云需在控制台预建词表，此处填写暂不生效"
        } else {
            "一行一个词（人名、品牌、术语），识别时优先命中"
        };
        ui.vertical(|ui| {
            ui.label(body("个人词典").strong().color(p.text));
            ui.label(small(desc).color(p.muted));
        });
        ui.add_space(4.0);
        ui.push_id("hotwords_editor", |ui| {
            ui.add_sized(
                [ui.available_width(), 72.0],
                egui::TextEdit::multiline(&mut self.edit.options.hotwords)
                    .hint_text("一行一个词，如：宝可梦、张三丰、BBVoxi"),
            );
        });
    }

    /// 通用页：快捷键 / 麦克风 / 开机自启 / 界面主题
    fn general_page(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            self.hotkey_row(ui, p);
            sep(ui, p);
            self.mic_row(ui, p);
            sep(ui, p);
            self.autostart_row(ui, p);
            sep(ui, p);
            self.theme_row(ui, p);
        });
    }

    /// 麦克风行：下拉栏列出本机所有能录音的设备。
    ///
    /// 显示的是 Windows 里那个好认的名字，配置里存的是设备唯一编号 ——
    /// 两台同型号麦克风的名字一模一样，只有编号分得开（见 `audio::MicDevice`）。
    fn mic_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // 先在布局之外把这一帧要显示的东西算好：横向布局里同时借 `self` 的
        // 好几个字段很难写下去，而这些值这一帧本来就不会变。
        let current = self.edit.options.mic_device.clone();
        let active = self
            .mics
            .iter()
            .find(|m| m.id == current)
            .map(|m| m.label.clone());
        let selected = if current.is_empty() {
            "系统默认".to_string()
        } else {
            // 选过但列表里没有 = 设备被拔了/停用了。下拉栏不装作"系统默认"
            // （那会和下面那行提醒自相矛盾），直接写明不在线
            active
                .clone()
                .unwrap_or_else(|| "（已选的麦克风不在线）".to_string())
        };
        // 悬停时要说的那句话：名字被截断时能看全；设备不在线时说清"下一步该干嘛"
        // （以前这里直接复用下拉栏里那串占位符，悬停出来只有"不在线"四个字，
        // 看不到那台设备本来叫什么，也不知道该怎么办）
        let hover = match (&active, current.is_empty()) {
            (Some(label), _) => label.clone(),
            (None, true) => "跟随 Windows 当前的默认录音设备".to_string(),
            (None, false) => {
                "你选的那台麦克风现在不在线（被拔掉或停用了）：插好后点「刷新」就能继续用它"
                    .to_string()
            }
        };
        let error = self.mics_error.clone();
        // 只能"确实列出来了、并且确实没有这一台"才敢说设备被拔掉/停用。
        // 枚举本身失败时列表是空的，那时候报"被拔掉"是编造原因（`audio` 那边真回退时
        // 也刻意只写"没能用上（已拔掉、被停用，或列表读不出来）"，就是怕猜错带偏排障）。
        let missing = error.is_none() && audio::selected_is_missing(&current, &self.mics);
        let none_listed = self.mics.is_empty();

        ui.horizontal(|ui| {
            let avail = ui.available_width();
            ui.vertical(|ui| {
                ui.set_min_width((avail - MIC_CTRL_W).max(120.0));
                ui.label(body("麦克风").strong().color(p.text));
                ui.label(small("录音用哪一台；默认跟随 Windows").color(p.muted));
            });
            let mut picked = current.clone();
            let combo_w = (ui.available_width() - MIC_BTN_W - INFO_W - 16.0).max(120.0);
            let combo = ui.scope(|ui| {
                // 把这一段的"可用宽度"压到预算内。为什么非压不可：
                // egui 0.35 的 `ComboBox::width` 只是**最小宽度**（combo_box.rs：
                // `actual_width = 文字宽 + 图标 … .at_least(minimum_width)`），而横向
                // 布局里按钮文字是 `Extend`（不换行、不截断）—— 截断长度取的是
                // "可用宽度"，不设上限它就一路铺到窗口右边缘。
                ui.set_max_width(combo_w);
                egui::ComboBox::from_id_salt("mic_device")
                    .width(combo_w)
                    // 必须显式截断：设备名一长（蓝牙耳机那种一大串），按钮就跟着变宽，
                    // 「刷新」和说明气泡会被挤出窗外 —— 按钮点不到，等于功能没了
                    .truncate()
                    .selected_text(body(selected.clone()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut picked, String::new(), "系统默认");
                        for m in &self.mics {
                            ui.selectable_value(&mut picked, m.id.clone(), m.label.as_str());
                        }
                    })
            });
            // 名字被截断时要能看全（下拉列表里也是全名）
            combo.inner.response.on_hover_text(&hover);
            if picked != current {
                self.edit.options.mic_device = picked;
            }
            if ghost_button(ui, p, "刷新").clicked() {
                self.refresh_mics();
                // 失败时**不能**报绿色的"已刷新"：底部一行绿字、下面一行红字，
                // 两个相反结论同屏打架，主人只会记住那个绿的
                match self.mics_error.clone() {
                    Some(e) => self.set_status(false, format!("列出麦克风失败：{e}")),
                    None => {
                        let n = self.mics.len();
                        self.set_status(true, format!("麦克风列表已刷新：{n} 台"));
                    }
                }
            }
            info(ui, p, TIP_MIC);
        });

        // 「选的那台现在不在」必须写出来：录音会自动退回系统默认，不说清楚的话
        // 主人只会觉得"我明明选了耳机麦克风，怎么还是用笔记本的麦"。
        if missing {
            ui.label(
                small("这台麦克风现在不在（已拔掉或被停用）：录音会自动改用系统默认设备")
                    .color(p.warn),
            );
        }
        if none_listed {
            ui.label(
                small(match &error {
                    Some(e) => format!("没能列出麦克风（{e}）"),
                    None => "没有检测到麦克风：插好设备后点「刷新」".to_string(),
                })
                .color(if error.is_some() { p.danger } else { p.muted }),
            );
        }
    }

    /// 界面主题：三选一，点了立即生效并落盘（改颜色不该还要点「保存」）
    fn theme_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let avail = ui.available_width();
            ui.vertical(|ui| {
                ui.set_min_width((avail - THEME_CTRL_W).max(110.0));
                ui.label(body("界面主题").strong().color(p.text));
                ui.label(small("跟随系统的深浅色，或固定一种").color(p.muted));
            });
            let picked = segmented(
                ui,
                p,
                p.card,
                &mut self.edit.theme,
                &THEME_MODES,
                theme_label,
                false,
            );
            if let Some(mode) = picked {
                self.apply_theme_now(ui.ctx().clone(), mode);
            }
        });
    }

    /// 主题切换的即时落盘逻辑（`theme_row` 只负责画）。
    ///
    /// 关键取舍：写进文件的是「上一次保存的配置 + 新主题」，**不是**编辑中的
    /// 整份 `edit` —— 否则主人填了一半还没保存的 API Key 会被顺手带进文件，
    /// 快捷键没校验过也会被存进去。主题是独立的小改动，值得单独走一趟。
    fn apply_theme_now(&mut self, ctx: egui::Context, mode: ThemeMode) {
        apply_theme(&ctx, mode);
        self.edit.theme = mode;
        let disk = theme_disk_copy(&self.saved, mode);
        match disk.save() {
            Ok(()) => {
                self.saved = disk;
                self.set_status(true, "主题已切换并保存");
            }
            Err(e) => {
                // 界面已经切了（主人看得见），只是没写进文件：如实报告，
                // 下次启动会回到旧主题 —— 但别的一句话也不说
                self.set_status(false, format!("主题已切换，但写配置文件失败：{e:#}"));
            }
        }
    }

    /// 凭据区：字段与标签同排（也取消了「高级折叠」—— 接口地址就是普通一行）
    fn credentials(&mut self, ui: &mut egui::Ui, p: &Palette) {
        match self.edit.provider {
            Provider::Qwen => {
                self.secret_row(ui, p, "API Key", "qwen_key", TIP_API_KEY);
                self.text_row(ui, p, "模型", "qwen_model", TIP_MODEL_QWEN);
                self.text_row(ui, p, "接口地址", "qwen_url", TIP_ENDPOINT_QWEN);
            }
            Provider::Doubao => {
                self.secret_row(ui, p, "API Key", "doubao_key", TIP_API_KEY);
                self.text_row(ui, p, "资源 ID", "doubao_res", TIP_RES_DOUBAO);
                // 高精度开关在「识别选项」页：它是识别行为，不是凭据
            }
            Provider::Tencent => {
                self.text_row(ui, p, "App ID", "tc_app", TIP_APP_ID);
                self.secret_row(ui, p, "Secret ID", "tc_sid", TIP_API_KEY);
                self.secret_row(ui, p, "Secret Key", "tc_skey", TIP_API_KEY);
                labeled_row(ui, "引擎模型", p, |ui| {
                    egui::ComboBox::from_id_salt("tencent_engine")
                        .selected_text(body(self.edit.tencent.engine_model_type.clone()))
                        .width((ui.available_width() - INFO_W - 8.0).max(80.0))
                        .show_ui(ui, |ui| {
                            for engine in TENCENT_ENGINES {
                                ui.selectable_value(
                                    &mut self.edit.tencent.engine_model_type,
                                    engine.to_string(),
                                    engine,
                                );
                            }
                        });
                    info(
                        ui,
                        p,
                        &format!(
                            "{TIP_TENCENT_ENGINE}\n\n内置接口 {TENCENT_URL}/<App ID>?签名参数（自动计算）"
                        ),
                    );
                });
            }
        }
    }

    /// 快捷键行：左边标题+一句话说明，右边键帽 + 「重新录制」。
    /// 完整的按键规则很长，留在悬停气泡里（`TIP_HOTKEY`）。
    fn hotkey_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            // 右侧固定留出：键帽 140 + 间距 + 按钮 + 说明气泡
            let avail = ui.available_width();
            ui.vertical(|ui| {
                ui.set_min_width((avail - HOTKEY_CTRL_W).max(120.0));
                ui.label(body("快捷键").strong().color(p.text));
                ui.label(small("按住说话，松开自动输入").color(p.muted));
            });
            if self.capturing {
                ui.label(body("按下新组合").color(p.warn).strong());
                if ghost_button(ui, p, "取消").clicked() {
                    self.end_capture();
                }
                ui.label(small("Esc 也可取消").color(p.muted));
                return;
            }
            let display = hotkey::parse(&self.edit.hotkey)
                .map(|h| h.display())
                .unwrap_or_else(|_| self.edit.hotkey.clone());
            // 键帽：固定宽 + 截断，超长组合（四个修饰键 + 主键）不会把按钮挤出窗口；
            // 完整内容悬停可见
            egui::Frame::new()
                .fill(p.field)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    // 等宽字体当键帽，反引号之类的符号更好认
                    ui.add_sized(
                        [140.0, 22.0],
                        egui::Label::new(
                            RichText::new(&display).monospace().strong().color(p.text),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&display);
                });
            if ghost_button(ui, p, "重新录制").clicked() {
                self.begin_capture(ui.ctx());
            }
            info(ui, p, TIP_HOTKEY);
        });
        if let Some(err) = self.hotkey_error.clone() {
            ui.label(small(err).color(p.danger));
        }
    }

    /// 自动更新的两个小窗：「发现新版本」和「装好了，要重启吗」。
    ///
    /// 为什么下载前一定要问一句（主人选的）：下载 + 替换自身是不可逆的动作，
    /// 而主人此刻可能正在干活；问一句的成本极低，装错了的代价很高。
    /// 点窗外空白/按 Esc = 取消（与关窗确认同一套规矩，绝不"悄悄开始更新"）。
    fn update_dialogs(&mut self, ctx: &egui::Context, p: &Palette) {
        match self.update.clone() {
            UpdateState::Found(rel) => {
                let mut choice: Option<bool> = None; // Some(true)=现在更新
                let modal =
                    egui::Modal::new(egui::Id::new("bbvoxi_update_found")).show(ctx, |ui| {
                        ui.set_width(360.0);
                        ui.label(
                            heading(format!("发现新版本 v{}", rel.version))
                                .strong()
                                .color(p.text),
                        );
                        ui.add_space(6.0);
                        ui.label(small(format!("你现在用的是 v{}", env!("CARGO_PKG_VERSION"))).color(p.muted));
                        ui.add_space(8.0);
                        let notes = update::notes_summary(&rel.notes, 400);
                        if !notes.trim().is_empty() {
                            ui.label(small("更新说明：").strong().color(p.muted));
                            ui.add_space(2.0);
                            ui.label(body(notes).color(p.text));
                            ui.add_space(8.0);
                        }
                        ui.label(
                            small("更新包会从 GitHub 下载，下载完自动核对指纹，装好后需要重启一次。")
                                .color(p.muted),
                        );
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ghost_button(ui, p, "取消").clicked() {
                                choice = Some(false);
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        button_text("现在更新").strong().color(p.on_accent),
                                    )
                                    .fill(p.accent)
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(104.0, 32.0)),
                                )
                                .clicked()
                            {
                                choice = Some(true);
                            }
                        });
                    });
                if modal.should_close() && choice.is_none() {
                    choice = Some(false);
                }
                match choice {
                    Some(true) => {
                        if let UpdateState::Found(rel) = self.update.clone() {
                            self.start_update_download(*rel);
                        }
                    }
                    Some(false) => {
                        self.update = UpdateState::Idle;
                        self.set_status(true, "已取消更新");
                    }
                    None => {}
                }
            }
            // 装好了：重启才生效。**绝不能自动重启** —— 主人可能正说话说到一半
            UpdateState::Installed { version } => {
                let mut choice: Option<bool> = None;
                let modal =
                    egui::Modal::new(egui::Id::new("bbvoxi_update_done")).show(ctx, |ui| {
                    ui.set_width(320.0);
                    ui.label(heading("更新已装好").strong().color(p.text));
                    ui.add_space(6.0);
                    ui.label(
                        body(format!("新版本 v{version} 已经就位，重启后开始使用。")).color(p.text),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        small("选「稍后」的话，你下次打开 BBVoxi 就是新版本了。").color(p.muted),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ghost_button(ui, p, "稍后").clicked() {
                            choice = Some(false);
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    button_text("立刻重启").strong().color(p.on_accent),
                                )
                                .fill(p.accent)
                                .corner_radius(CornerRadius::same(8))
                                .min_size(egui::vec2(104.0, 32.0)),
                            )
                            .clicked()
                        {
                            choice = Some(true);
                        }
                    });
                });
                // 点窗外空白 / 按 Esc = 「稍后」（与关窗确认同一套规矩）。
                // 少了这一句，主人按 Esc 想"晚点再说"，弹窗纹丝不动 —— 而且这个状态
                // 会一直留着，下次打开设置窗又弹一遍。
                if modal.should_close() && choice.is_none() {
                    choice = Some(false);
                }
                match choice {
                    Some(true) => self.restart_into_new_version(),
                    // 稍后：把状态收掉，底部反馈条上留一句"重启后生效"
                    Some(false) => {
                        self.update = UpdateState::Idle;
                        self.set_status(true, format!("v{version} 已装好，下次启动生效"));
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// 关窗确认（有改动还没保存时才出现）。
    ///
    /// 为什么值得做：配置改动只在内存里，只有点「保存」才落盘；旧版点 ✕ 直接隐藏
    /// 窗口 —— 改了半天 API Key / 快捷键，再打开全没了，而且一声不吭（报告第 2 条）。
    /// 三个出口都摆在明面上，选「不保存」也是主人自己的决定。
    fn close_confirm(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.confirm_close {
            return;
        }
        enum Choice {
            Save,
            Discard,
            Cancel,
        }
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("bbvoxi_close_confirm")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(heading("有改动还没保存").strong().color(p.text));
            ui.add_space(6.0);
            ui.label(body("直接关掉的话，这次改的内容就没了。").color(p.muted));
            // 「保存并关闭」失败过（比如快捷键格式不对）就把原因显示在对话框里：
            // 对话框是前景层，会把底部的状态条盖住，主人否则看不到"为什么没关掉"
            if let Some(status) = &self.status {
                if !status.ok {
                    ui.add_space(6.0);
                    ui.label(body(&status.text).color(p.danger));
                }
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, p, "取消").clicked() {
                    choice = Some(Choice::Cancel);
                }
                if ghost_button(ui, p, "不保存").clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui
                    .add(
                        egui::Button::new(button_text("保存并关闭").strong().color(p.on_accent))
                            .fill(p.accent)
                            .corner_radius(CornerRadius::same(8))
                            .min_size(egui::vec2(104.0, 32.0)),
                    )
                    .clicked()
                {
                    choice = Some(Choice::Save);
                }
            });
        });
        // 点背后空白 / 按 Esc = 取消（绝不等于"悄悄把改动丢掉"）
        if modal.should_close() && choice.is_none() {
            choice = Some(Choice::Cancel);
        }
        match choice {
            // 保存失败（快捷键格式不对等）时对话框留着、错误就显示在下面，
            // 绝不把改动悄悄丢掉
            Some(Choice::Save) => {
                if self.save() {
                    self.confirm_close = false;
                    self.hide_requested = true;
                }
            }
            Some(Choice::Discard) => {
                self.discard_changes();
                self.confirm_close = false;
                self.hide_requested = true;
            }
            Some(Choice::Cancel) => self.confirm_close = false,
            // 对话框开着、主人还没选
            None => {}
        }
    }

    /// 把没保存的改动真的丢掉（关窗确认里选「不保存」时走这里）。
    ///
    /// 为什么不能只清对话框标志：设置窗实例整个进程只建一次（窗口只是 hide/show），
    /// `edit` 一直留着 —— 下次打开设置窗，被丢弃的改动会原样回来，主人再改一处
    /// 一点「保存」，它们就一起落盘了。对话框承诺的是"这次改的内容就没了"。
    ///
    /// 快捷键还得**单独恢复**：改键是立即生效的（`commit_hotkey` 里 `set_current`），
    /// 不恢复的话界面显示旧键、钩子却认新键 —— 按旧键毫无反应，重启又变回去。
    fn discard_changes(&mut self) {
        self.edit = self.saved.clone();
        self.hotkey_error = None;
        self.status = None;
        if let Err(e) = hotkey::apply_from_config(&self.saved.hotkey) {
            // 配置文件里那个键本来就该是合法的；真出问题也要留痕，别装没看见
            self.hotkey_error = Some(format!("恢复原快捷键失败：{e}"));
        }
    }

    /// 开机自启行：勾选立即写注册表（失败会回滚开关）
    fn autostart_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let before = self.autostart;
        setting_row(
            ui,
            p,
            "开机自启",
            "登录 Windows 后自动在后台待命，不弹窗口",
            &mut self.autostart,
            true,
        );
        if self.autostart != before && self.autostart != self.applied_autostart {
            match autostart::set(self.autostart) {
                Ok(()) => {
                    self.applied_autostart = self.autostart;
                    self.set_status(
                        true,
                        if self.autostart {
                            "已开启开机自启"
                        } else {
                            "已关闭开机自启"
                        },
                    );
                }
                Err(e) => {
                    self.autostart = self.applied_autostart;
                    self.set_status(false, format!("{e:#}"));
                }
            }
        }
    }

    /// 底部反馈条里那一句"更新到哪一步了"（没有更新动作时返回 `None`，走原来的优先级链）。
    ///
    /// 抽成纯函数：`update_line` 里的百分比换算最容易写错（总字节未知时不能除零，
    /// 也不能显示一个假的百分比）。
    fn update_line(&self, p: &Palette) -> Option<(String, Color32)> {
        match &self.update {
            UpdateState::Idle => None,
            UpdateState::Checking => Some(("正在检查新版本…".to_string(), p.text)),
            UpdateState::Found(rel) => {
                Some((format!("发现新版本 v{}，等你确认", rel.version), p.text))
            }
            UpdateState::Downloading { done, total, .. } => Some((
                format!("正在下载新版本 {}", download_progress(*done, *total)),
                p.text,
            )),
            UpdateState::Installed { version } => {
                Some((format!("v{version} 已装好，重启后生效"), p.ok))
            }
            UpdateState::Restarting => Some(("正在启动新版本…".to_string(), p.text)),
            // 失败**不在这里显示**：它已经作为一条状态消息说出去了（那条会自然退场），
            // 留在这里会把之后所有状态消息（保存成功、麦克风刷新…）永久压住。
            UpdateState::Failed(_) => None,
        }
    }

    /// 「最近识别」紧凑反馈条（钉在底部操作条上方，任何页签都看得见）。
    ///
    /// 为什么做成单行：它是个"顺手瞄一眼"的地方，不是阅读区；测试识别的按钮
    /// 就在正下方，结果贴着按钮放，视线不用跳。完整内容本来就在剪贴板
    /// （识别成功的场合），截断不丢信息。测试结果不会外打是主人最需要
    /// 确认的一件事，所以这一条写在标签后面、不进气泡。
    fn result_strip(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        let hotkey_text = hotkey::parse(&self.edit.hotkey)
            .map(|h| h.display())
            .unwrap_or_else(|_| self.edit.hotkey.clone());
        egui::Frame::new()
            .fill(p.field)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(small("最近识别").strong().color(p.muted));
                    ui.add_space(4.0);
                    // 状态优先展示：录音中 > 出错 > 提示 > 正文
                    if snap.recording {
                        // 录音中的呼吸点：整窗唯一的动效，一眼看出"正在听"
                        let t = ui.ctx().input(|i| i.time);
                        let pulse = (0.55 + 0.45 * (t * 4.0).sin()) as f32;
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                        ui.painter().circle_filled(
                            rect.center(),
                            4.0,
                            p.danger.gamma_multiply(0.35 + 0.65 * pulse),
                        );
                        ui.label(
                            body(format!("录音中 {}", clock(snap.elapsed())))
                                .strong()
                                .color(p.danger),
                        );
                    }
                    let (text, color) = if snap.recording {
                        let live = if !snap.text.is_empty() {
                            snap.text.clone()
                        } else if !snap.hint.is_empty() {
                            snap.hint.clone()
                        } else {
                            "请说话…".to_string()
                        };
                        (live, p.text)
                    } else if let Some(err) = &snap.error {
                        (err.clone(), p.danger)
                    } else if let Some((line, color)) = self.update_line(p) {
                        // 更新进度紧跟"识别错误"之后、普通提示之前：主人刚点了按钮，
                        // 这是他此刻最关心的一句话（"正在下载 42%"）
                        (line, color)
                    } else if let Some(status) = &self.status {
                        // 界面操作的反馈（已保存 / 保存失败 / 已开始测试…）也走这条：
                        // 它就是底部固定的反馈位。放在识别错误之后 —— 识别失败更要紧。
                        (status.text.clone(), if status.ok { p.ok } else { p.danger })
                    } else if let Some(notice) = &snap.notice {
                        // 结果已放进剪贴板（通常是录音途中切了窗口）。用提醒色而不是
                        // 危险色：这不是故障，但主人得知道"这次的字没自动打出去，去
                        // Ctrl+V 粘"。
                        (notice.clone(), p.warn)
                    } else if !snap.last_result.is_empty() {
                        let mut s = snap.last_result.clone();
                        // 「已留剪贴板」只在真的留住了（写完回读核对过）才说：
                        // 复制失败的那一次也写，主人去 Ctrl+V 却什么也粘不出来
                        if snap.kept_on_clipboard {
                            s.push_str("（已留剪贴板）");
                        }
                        (s, p.text)
                    } else {
                        (
                            format!("还没有记录。按住 {hotkey_text} 说句话，或点「测试识别」"),
                            p.muted,
                        )
                    };
                    // 单行截断：长文本不换行、不撑高操作条
                    ui.add(egui::Label::new(body(text).color(color)).truncate())
                        .on_hover_text("只显示在这里，不会打字出去");
                });
            });
    }

    // —— 底部操作条 ——

    fn actions_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // 状态消息显示在上方反馈条里（见 `result_strip` 的优先级链），
        // 按钮行高度因此恒定 —— 页脚总高也随之恒定，滚动区才能安全地
        // 用"可用高度减页脚"来限高（见 `ui` 里的 `FOOTER_H`）。
        let recording = self.shared.snapshot().recording;
        // 按钮宽度为什么收紧成这一套：更新失败时会临时多出「手动下载」（第 5 个
        // 按钮），而最窄窗口（520）下这一行要同时装下 5 个按钮 + 版本号。
        // 原来「测试识别（5 秒）」那个 130 宽撑不下 —— 而底栏放不下的第一件事，
        // 就是把最右边的版本号顶出窗口。所以：标签去掉"（5 秒）"（挪进悬停提示），
        // 其余按钮按"文字宽 + 内边距"给最小宽度，全部有布局测试兜住。
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !recording,
                    egui::Button::new(button_text("测试识别").color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(76.0, 34.0)),
                )
                .on_hover_text("点一下固定录 5 秒，结果只显示在窗口里（不会往外打字）")
                .clicked()
            {
                if !self.edit.credentials_ready() {
                    self.set_status(false, "请先填写当前服务商的凭据");
                } else {
                    self.sync_to_shared();
                    let _ = self.cmd_tx.send(Cmd::Test);
                    self.set_status(true, "已开始 5 秒测试");
                }
            }
            // 有没保存的改动时，按钮右上角点一个小圆点：不用等关窗提醒，
            // 一眼就知道"这里攒着东西没落盘"
            let save_btn = ui.add(
                egui::Button::new(button_text("保存").strong().color(p.on_accent))
                    .fill(p.accent)
                    .corner_radius(CornerRadius::same(8))
                    .min_size(egui::vec2(64.0, 34.0)),
            );
            if self.has_unsaved_changes() {
                let r = save_btn.rect;
                ui.painter()
                    .circle_filled(egui::pos2(r.right() - 5.0, r.top() + 5.0), 3.5, p.on_accent);
            }
            if save_btn.clicked() {
                self.save();
            }
            if ui
                .add(
                    egui::Button::new(button_text("项目地址").color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(84.0, 34.0)),
                )
                .clicked()
            {
                if let Err(e) = open_project_page() {
                    self.set_status(false, format!("打开项目地址失败：{e}"));
                }
            }
            // 「检查更新」就摆在「项目地址」右边。更新期间按钮变灰 + 换字，
            // 免得连点几次同时跑好几个下载。
            let busy = matches!(
                self.update,
                UpdateState::Checking
                    | UpdateState::Downloading { .. }
                    | UpdateState::Restarting
            );
            let label = match self.update {
                UpdateState::Checking => "检查中…",
                UpdateState::Downloading { .. } => "下载中…",
                UpdateState::Restarting => "重启中…",
                _ => "检查更新",
            };
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(button_text(label).color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(84.0, 34.0)),
                )
                .on_hover_text("去 GitHub 看看有没有新版本")
                .clicked()
            {
                self.start_update_check();
            }
            // 更新失败时多给一个出口：直接去 GitHub 发布页手动下载。
            // 网络环境千奇百怪，自动更新不可能 100% 成功，主人得有条退路。
            if matches!(self.update, UpdateState::Failed(_))
                && ui
                    .add(
                        egui::Button::new(button_text("手动下载").color(p.text))
                            .fill(p.field)
                            .stroke(Stroke::new(1.0, p.border))
                            .corner_radius(CornerRadius::same(8))
                            .min_size(egui::vec2(84.0, 34.0)),
                    )
                    .on_hover_text("用浏览器打开 GitHub 发布页，自己下新的 exe")
                    .clicked()
            {
                if let Err(e) = open_url(&update::releases_page()) {
                    self.set_status(false, format!("打开下载页失败：{e}"));
                }
            }
            // 版本号钉在按钮行最右边：主人靠它分辨"我测的到底是不是新版"
            // （版本号本身不能省，所以按钮宽度才要收紧，见这一行开头的说明）。
            small_at_end(ui, p, concat!("v", env!("CARGO_PKG_VERSION")));
        });
    }

    // —— 行为 ——

    /// 保存当前编辑内容；返回是否成功（失败原因写进状态消息 / `hotkey_error`）。
    ///
    /// 返回 bool 是为了关窗确认：选了「保存并关闭」但保存失败时，必须留在窗口里
    /// 把错误显示出来，不能把改动悄悄丢掉。
    fn save(&mut self) -> bool {
        // 快捷键要立刻生效，不能等重启（否则用户改完按了没反应，以为坏了）
        match hotkey::apply_from_config(&self.edit.hotkey) {
            Ok(hk) => {
                self.edit.hotkey = hk.to_config();
                self.hotkey_error = None;
            }
            Err(e) => {
                self.hotkey_error = Some(format!("快捷键无效：{e}"));
                self.set_status(false, "快捷键格式不对，未保存");
                return false;
            }
        }
        match self.edit.save() {
            Ok(()) => {
                self.sync_to_shared();
                // 记下"这一份已经落盘了"：关窗前"有没有没保存的改动"就靠它
                self.saved = self.edit.clone();
                self.set_status(true, "已保存");
                true
            }
            Err(e) => {
                self.set_status(false, format!("保存失败：{e:#}"));
                false
            }
        }
    }

    fn sync_to_shared(&self) {
        if let Ok(mut guard) = self.shared_cfg.lock() {
            *guard = self.edit.clone();
        }
    }

    fn begin_capture(&mut self, ctx: &egui::Context) {
        self.capturing = true;
        self.hotkey_error = None;
        // 上一轮留下的半截修饰键状态，不能带到这一轮来
        self.pending_mods = None;
        self.paused.store(true, Ordering::Relaxed);
        // 捕捉期间钩子是暂停的，按键会原样进到窗口。
        // 若这之前焦点还停在某个输入框（比如密钥框），按下的字母会被插进去，
        // 悄悄改坏配置 —— 所以先把键盘焦点收回来。
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }

    fn end_capture(&mut self) {
        self.capturing = false;
        self.pending_mods = None;
        self.paused.store(false, Ordering::Relaxed);
    }

    /// 记下/应用一个新录到的快捷键。
    ///
    /// 统一走 `parse(to_config())` 当校验器，而不是直接信任录到的东西：
    /// 录出来的组合有可能**不合法**（比如只按下一个 Shift），
    /// 校验器就是唯一的把关口，跟"主人手打配置字符串"走的是同一套规则。
    fn commit_hotkey(&mut self, candidate: Hotkey) {
        self.pending_mods = None;
        match hotkey::parse(&candidate.to_config()) {
            Ok(hk) => {
                self.edit.hotkey = hk.to_config();
                hotkey::set_current(hk);
                self.hotkey_error = None;
                self.end_capture();
                self.set_status(true, format!("快捷键已改为 {}", hk.display()));
            }
            Err(e) => self.hotkey_error = Some(e.to_string()),
        }
    }

    fn handle_capture(&mut self, ctx: &egui::Context) {
        if !self.capturing {
            return;
        }
        // 窗口失去焦点就退出捕捉：否则钩子会一直停在「暂停」状态，按什么都没反应
        if !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
            self.end_capture();
            return;
        }
        // 捕捉期间得持续重绘，否则 eframe 只按需泵消息，
        // `current_mods()` 的变化就看不到、松手那一刻会漏掉
        ctx.request_repaint();

        // 主键只从 egui 事件里取；修饰键一律读钩子。
        //
        // 为什么不读 `i.modifiers`：egui 的 `Modifiers` 里**根本没有 Win 这一项**，
        // 它只知道 ctrl/alt/shift/mac_cmd —— 靠它录 Ctrl+Win 会永远缺一块。
        // 钩子那边左右分开的键码都认得（见 `hotkey::MODIFIERS`），所以以它为准。
        let key = ctx.input(|i| take_main_key(&i.events));
        // Esc 单独按下 = 取消录制（符合主人直觉）。但**带着修饰键的 Esc 是主键**：
        // `MAIN_KEYS` 里本来就有 Esc，之前这里一刀切地取消，导致 Ctrl+Esc 这类
        // 组合永远录不进去（表里说支持、实际录不到，属于名不副实）。
        if key == Some(egui::Key::Escape) && hotkey::current_mods() == hotkey::Mods::default() {
            self.end_capture();
            return;
        }
        let main = match key {
            Some(k) => match hotkey::egui_key_to_vk(k) {
                Some(vk) => Some(vk),
                None => {
                    self.hotkey_error = Some(
                        "这个按键不能当快捷键，请换一个（字母、数字、F1~F24、方向键或符号）".into(),
                    );
                    self.pending_mods = None;
                    return;
                }
            },
            None => None,
        };
        if let Some(hk) = capture_step(hotkey::current_mods(), main, &mut self.pending_mods) {
            self.commit_hotkey(hk);
        }
    }

    // —— 字段 ——

    /// 普通文本字段一行（标签左、输入框右、说明进气泡）
    fn text_row(&mut self, ui: &mut egui::Ui, p: &Palette, label: &str, id: &str, tip: &str) {
        labeled_row(ui, label, p, |ui| {
            let value = match id {
                "qwen_model" => &mut self.edit.qwen.model,
                "qwen_url" => &mut self.edit.qwen.base_url,
                "doubao_res" => &mut self.edit.doubao.resource_id,
                "tc_app" => &mut self.edit.tencent.app_id,
                _ => return,
            };
            let width = (ui.available_width() - INFO_W - 8.0).max(80.0);
            field_edit(ui, value, false, width);
            info(ui, p, tip);
        });
    }

    /// 密钥字段一行：输入框右侧贴一个"显示/隐藏"（入口紧贴字段本身）
    fn secret_row(&mut self, ui: &mut egui::Ui, p: &Palette, label: &str, id: &str, tip: &str) {
        labeled_row(ui, label, p, |ui| {
            let value = match id {
                "qwen_key" => &mut self.edit.qwen.api_key,
                "doubao_key" => &mut self.edit.doubao.api_key,
                "tc_sid" => &mut self.edit.tencent.secret_id,
                "tc_skey" => &mut self.edit.tencent.secret_key,
                _ => return,
            };
            let reserved = EYE_W + 8.0 + INFO_W + 8.0;
            let width = (ui.available_width() - reserved).max(80.0);
            field_edit(ui, value, !self.show_keys, width);
            let eye = if self.show_keys { "隐藏" } else { "显示" };
            if small_button(ui, p, eye).clicked() {
                self.show_keys = !self.show_keys;
            }
            info(ui, p, tip);
        });
    }
}

// —— 无状态小部件 ——

/// 卡片：白底 + 一点柔和投影（egui 0.35 自带的能力，替代"靠 1px 边框硬撑"）
fn card<R>(p: &Palette, ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui, &Palette) -> R) -> R {
    // 投影只在浅色主题下明显；深色主题底下用黑色投影基本看不见，干脆更淡一点
    let shadow = if ui.visuals().dark_mode {
        Color32::from_black_alpha(90)
    } else {
        Color32::from_black_alpha(14)
    };
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .shadow(egui::Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: shadow,
        })
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui, p)
        })
        .inner
}

// —— 排版常量与语义字号 ——

/// 设置卡里"标签列"的宽度：所有行都从这个位置开始排控件，右侧自然对齐成一条线
const LABEL_W: f32 = 56.0;
/// 说明气泡图标的宽度
const INFO_W: f32 = 18.0;
/// 开关（`toggle_glyph`）的宽度
const TOGGLE_W: f32 = 40.0;
/// 主题行右侧给三段选择器预留的宽度（跟随系统 / 浅色 / 深色）。
/// 行内模式每段 80、段间 gap 6 + item_spacing 8 → 实际 3×80+2×14=268，
/// 再加左列与选择器之间的一格 item_spacing，预算 280 才够 —— 预算偏小时
/// 左列不收缩、行总宽超卡片，会把整张卡撑出窗口（实测过）。
const THEME_CTRL_W: f32 = 280.0;
/// 快捷键行右侧给「键帽 + 重新录制 + 说明气泡」预留的宽度。
/// 键帽框 162 + 间距 8 + 按钮 96 + 间距 8 + 气泡 18 ≈ 292，加上左列与
/// 右侧之间一格 item_spacing，留 320 —— 宁可左侧说明列窄一点，也不能
/// 让行总宽超过卡片。
const HOTKEY_CTRL_W: f32 = 320.0;
/// 通用页「麦克风」行右侧的宽度预算：下拉栏 190 + 间距 8 + 「刷新」按钮 52 +
/// 间距 8 + 说明气泡 18 ≈ 276，再加左列与右侧之间一格 item_spacing，留 292。
/// 下拉栏得留够：设备名最长的是「麦克风 (BY-CM1)」这种，窄了会被截成一串问号，
/// 主人就没法确认自己选的是不是想要的那台。
const MIC_CTRL_W: f32 = 292.0;
/// 「刷新」按钮的宽度预算（`ghost_button` 的宽度 = 文字宽 + 左右内边距 24）
const MIC_BTN_W: f32 = 52.0;
/// 页脚（分隔线 + 「最近识别」反馈条 + 按钮行）占的高度。
/// 滚动区用它限高（`ui` 里 `available_height() - FOOTER_H`），页脚才能钉在
/// 窗口底。这个值必须 ≥ 页脚真实高度，宁可略大（大一点只是页脚上方多点留白，
/// 小了按钮行会被窗口底裁掉一截）。按钮行没有动态的状态行，高度恒定；
/// 实测构成：分隔线区 14 + 反馈条 44 + 间距 6 + 按钮行 34 = 106，取 108。
const FOOTER_H: f32 = 108.0;
/// 说明气泡里那个 "i" 的字号。
///
/// 图标不是正文：它要配出一个 13px 的小圆，所以**故意**不跟字号档位走
/// （字号收敛是给文字用的，不是给图标硬套一个语义档）。
const INFO_GLYPH_SIZE: f32 = 10.0;
/// 「显示/隐藏」小按钮的宽度
const EYE_W: f32 = 48.0;

/// 标题档（品牌名、对话框标题）
fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(egui::TextStyle::Heading)
}

/// 正文档（默认档位：字段文字、复选框、结果文本）
fn body(text: impl Into<String>) -> RichText {
    RichText::new(text)
}

/// 按钮档
fn button_text(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(egui::TextStyle::Button)
}

/// 说明档（小字：状态胶囊、版本号、次要提示）
fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(egui::TextStyle::Small)
}

// —— 悬停说明（原来堆在页面上的那些小字，全在这里）——
//
// 说明不是不重要，是**没地方放**：旧版把它们全摊在页面上，占了卡片总高的 30%，
// 页面被撑到要滚 1.4 屏。收进气泡之后一句话都没少，页面却干净了（报告第 8 条）。

const TIP_API_KEY: &str = "在所选服务商的控制台里创建 API Key / 密钥，粘贴到这里。\
「显示/隐藏」只影响这一行怎么显示。";

const TIP_MODEL_QWEN: &str = "千问的模型名，不确定就保持默认\
（qwen-audio-3.0-asr-flash-streaming）。";

const TIP_ENDPOINT_QWEN: &str = "接口地址，一般不用改：默认是阿里云百炼的官方地址，\
只有开通了专属业务空间才需要换。";

const TIP_RES_DOUBAO: &str = "火山引擎的资源 ID（控制台里创建实例时给的那串），不确定就保持默认。";

const TIP_APP_ID: &str = "腾讯云语音识别的 App ID（控制台里的数字 ID）。";

const TIP_TENCENT_ENGINE: &str = "腾讯云的识别引擎。Hy-ASR-3.0-preview 只支持 60 秒以内的语音\
（建议说到 55 秒就停）；16k_zh_en 支持中英混说。";

const TIP_HOTKEY: &str = "修饰键 Ctrl / Alt / Shift / Win 随意搭配（1~4 个都行），主键支持字母、\
数字、F1~F24、空格、方向键、翻页键和符号键；也可以整组只用修饰键（如 Ctrl + Win，先按住 \
Ctrl 再按 Win，全部松开后生效）。按下时会拦截该组合键。\n\n\
只有单独一个普通键（如 a）和单独一个修饰键（如 Ctrl）不能当快捷键：\
前者会在所有程序里吞掉这个键，后者的「按住说话」和 Ctrl+C 分不开。";

const TIP_MIC: &str = "「系统默认」= 跟着 Windows 当前的默认录音设备走（换耳机、插音箱都不用回来改），\
推荐保持这一项。指定某一台之后，每次录音就只用这一台。\n\n\
选中的设备被拔掉或被停用时，录音会自动退回系统默认设备（那一行会亮起提醒，日志里也有记录），\
不会因为选错设备就录不出声音来。刚插上的新设备点「刷新」即可出现在列表里。";

// 说明文字现在直接写在每行设置卡上（见 `setting_row`）；「识别选项」「通用」
// 两页不再需要悬停气泡，上面的常量只剩凭据与快捷键在用。

/// 设置卡里的一行：左边标签、右边控件。
///
/// 标签列宽度**写死**（`LABEL_W`），所有行的控件都从同一条线开始 —— 用
/// `allocate_ui_with_layout` 只能"按内容宽度占位"（实测），所以必须用
/// `set_min_width` 把这一列真正占住。
fn labeled_row(
    ui: &mut egui::Ui,
    label: impl Into<String>,
    p: &Palette,
    add: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.scope(|ui| {
            ui.set_min_width(LABEL_W);
            ui.label(body(label).color(p.muted));
        });
        add(ui);
    });
}

/// 下载进度的人话：知道总大小时给"42%（3.5/8.0 MB）"，不知道就给已下多少。
///
/// 为什么单独抽出来：服务器没给 Content-Length 时总字节是 0，算百分比会除零；
/// 而"42%"这种数字写错了，主人一眼就能看出来 —— 值得钉一条测试。
fn download_progress(done: u64, total: u64) -> String {
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    if total == 0 {
        format!("{:.1} MB", mb(done))
    } else {
        let pct = (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
        format!("{pct:.0}%（{:.1}/{:.1} MB）", mb(done), mb(total))
    }
}

/// 一段文字在某个字号档下的宽度（自己量，不依赖 egui 的右对齐布局）
fn text_width(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER)
        .size()
        .x
}

/// 把后面的内容推到行尾：先补足空白，再画。
///
/// 为什么不用 egui 的 `Layout::right_to_left` / `egui::Sides`：实测（见
/// `layout::no_text_overflows_the_window`）它们在"横向行里再套一层"时会
/// **把控件画到容器外**，右对齐的说明文字因此被窗口边缘切掉。自己量宽度、
/// 自己补空白，位置完全可控。
///
/// 为什么要扣一格 item_spacing：`add_space(pad)` 之后 egui 还会在下一个
/// widget 之前自动加 `item_spacing.x`（默认 8px）。不扣的话"推到行尾"的
/// 控件实际右沿会超出 max_rect —— 状态胶囊就因此把整个面板的 max_rect
/// 撑宽了 4px，后面每一层（页签、卡片、版本号）全都跟着右移出窗。
fn push_to_end(ui: &mut egui::Ui, width: f32) {
    let item = ui.style().spacing.item_spacing.x;
    let pad = (ui.available_width() - width - item).max(0.0);
    ui.add_space(pad);
}

/// 行尾的小字（自己算位置，见 `push_to_end`）
fn small_at_end(ui: &mut egui::Ui, p: &Palette, text: &str) {
    let w = text_width(ui, text, egui::TextStyle::Small);
    push_to_end(ui, w);
    ui.label(small(text).color(p.muted));
}

/// 一行设置（行卡片）：左边标题 + 说明两行字，右边一个开关，**整行可点**。
///
/// 为什么说明要摆到明面上：旧版把说明全收进悬停气泡，不把鼠标停在小圆圈上
/// 就不知道开关是干嘛的；说明跟着标题走，「直观」就从这里来。
/// 禁用时说明换成"为什么不支持"，而不是给一个拨了没反应的开关。
fn setting_row(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    desc: &str,
    value: &mut bool,
    enabled: bool,
) -> bool {
    let mut toggled = false;
    ui.push_id(title, |ui| {
        // 行整体可点（含开关区域）：开关只是行内的一个"显示件"，
        // 点击统一在这一层处理，不会出现"点了开关切换两次"。
        // egui 0.35 的 Frame 没有 sense()，所以先画再对整个区域 interact。
        let inner = egui::Frame::new()
            .corner_radius(CornerRadius::same(8))
            // 上下 5：行距预算紧（Options 页 5+ 行开关 + 语言行），每行省 2px
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let avail = ui.available_width();
                    ui.vertical(|ui| {
                        ui.set_min_width((avail - TOGGLE_W - 12.0).max(120.0));
                        ui.label(body(title).strong().color(p.text));
                        ui.label(small(desc).color(p.muted));
                    });
                    toggle_glyph(ui, p, *value, enabled);
                });
            });
        let resp = ui.interact(
            inner.response.rect,
            ui.id().with("row"),
            egui::Sense::click(),
        );
        if enabled {
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                *value = !*value;
                toggled = true;
            }
        }
    });
    toggled
}

/// 开关的显示件（iOS 风格轨道 + 圆点，滑动有 0.12s 过渡）。
///
/// 只画、不响应点击（点击由所在行统一处理，见 `setting_row`）。
/// egui 自带的 Checkbox 在这里不合适：方框 + 文字的宽度不受控，
/// 右对齐会被挤歪，而且视觉上更像"打勾的清单"而不是"拨的开关"。
fn toggle_glyph(ui: &mut egui::Ui, p: &Palette, on: bool, enabled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(TOGGLE_W, 22.0), egui::Sense::hover());
    // 动画值 0..1：egui 自带的插值，同一个 id 下每帧自动推进
    let v = ui
        .ctx()
        .animate_bool_with_time(ui.id().with("knob"), on, 0.12);
    let track = if enabled && on { p.accent } else { p.field };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(11), track);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(1.0, if enabled && on { p.accent } else { p.border }),
        egui::StrokeKind::Middle,
    );
    let d = 16.0;
    let pad = 3.0;
    let x0 = rect.left() + pad + d / 2.0;
    let x1 = rect.right() - pad - d / 2.0;
    let cx = egui::lerp(x0..=x1, v);
    let dot = if !enabled {
        p.muted.gamma_multiply(0.6)
    } else if on {
        p.on_accent
    } else {
        p.muted
    };
    ui.painter()
        .circle_filled(egui::pos2(cx, rect.center().y), d / 2.0, dot);
}

/// 说明气泡：一个不起眼的小圆圈，鼠标停上去才展开一句话
fn info(ui: &mut egui::Ui, p: &Palette, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(INFO_W, 16.0), egui::Sense::hover());
    let center = rect.center();
    ui.painter()
        .circle_stroke(center, 6.5, Stroke::new(1.0, p.muted));
    // 用画笔画一个 "i"，而不是依赖字体里有没有 ⓘ 这个字符（缺字形会显示成方块）
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(INFO_GLYPH_SIZE),
        p.muted,
    );
    resp.on_hover_text(tip)
}

/// 卡片内部的分组分隔线（比"再开一张卡片"省一大截高度）
fn sep(ui: &mut egui::Ui, p: &Palette) {
    ui.add_space(8.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(0), p.border);
    ui.add_space(8.0);
}

fn field_edit(ui: &mut egui::Ui, value: &mut String, password: bool, width: f32) {
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(width)
            .password(password)
            .margin(Margin::symmetric(10, 6)),
    );
}

fn ghost_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(button_text(text).color(p.text))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// 贴在字段标签行里的小按钮（如"显示/隐藏"密钥）
fn small_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(small(text).color(p.muted))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(6))
            .min_size(egui::vec2(0.0, 22.0)),
    )
}

/// 改键捕捉是否已经失效（界面久久没被绘制）。
///
/// 抽成纯函数只是为了能把 `CAPTURE_IDLE_LIMIT` 这条边界钉死在单测里 ——
/// 这里判错的代价很不对称：判漏了，`paused` 会永远留着、**全局快捷键彻底失效**；
/// 判早了只是让主人重新点一次「重新录制」。所以边界取 ">"，宁可晚一步收。
fn capture_is_stale(capturing: bool, idle: Duration) -> bool {
    capturing && idle > CAPTURE_IDLE_LIMIT
}

/// 改键捕捉走一步，返回录完的快捷键（还没录完返回 `None`）。
///
/// 抽成纯函数：录键牵扯「按了修饰键还没松」「先松主键还是先松修饰键」这些时序，
/// 只有能单测才敢说它是对的。真正碰系统的部分（钩子状态、egui 事件）都在调用方，
/// 这里只做判断。
///
/// 参数：
/// * `mods` —— 此刻真正按住的修饰键（来自钩子，含 Win）
/// * `main` —— 这一帧新按下的**主键**虚拟键码（没有就是 `None`）
/// * `pending` —— 跨帧记忆：手指全松开那一刻，靠它拼出纯修饰键组合
///
/// 两条出口：
/// 1. 出现了主键 → 立刻成组合（Ctrl+Win 归修饰键，主键就是被按的那个）；
/// 2. 没有任何修饰键按住、但 `pending` 里有东西 → 说明刚才握着一把修饰键全松手了，
///    补上主键（`vk = 0`）成纯修饰键组合。合法性交给 `commit_hotkey` 里的校验器。
///
/// **`pending` 只增不减**（关键，Ctrl+Win 录不上的另一半原因）：手指是一根根
/// 松开的，`mods` 会逐帧变小；若每帧都用"当前按住的"覆盖记忆，最后留下的只是
/// **最后松开的那根手指**（Ctrl+Win 会变成"只剩 Win"），校验器当然不认 ——
/// 表现为"我明明按了 Ctrl+Win，却提示至少要两个修饰键"。所以只记住凑齐得
/// 最多的那一刻：数量更多才替换，同样多则取更晚的（中间误碰一下别的修饰键，
/// 之后按的组合仍然盖得过去）。
fn capture_step(
    mods: hotkey::Mods,
    main: Option<u32>,
    pending: &mut Option<hotkey::Mods>,
) -> Option<Hotkey> {
    let as_hotkey = |m: hotkey::Mods, vk: u32| Hotkey {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        win: m.win,
        vk,
    };
    if let Some(vk) = main {
        return Some(as_hotkey(mods, vk));
    }
    let count = |m: hotkey::Mods| m.ctrl as u8 + m.alt as u8 + m.shift as u8 + m.win as u8;
    if mods != hotkey::Mods::default() {
        // 用 `map_or` 不用 `is_none_or`：后者是 Rust 1.82 才有的 API，
        // 本工程声明的最低版本是 1.80（写在 Cargo.toml 里），不能指标。
        if pending.map_or(true, |p| count(mods) >= count(p)) {
            *pending = Some(mods);
        }
        return None;
    }
    pending.take().map(|m| as_hotkey(m, 0))
}

/// 从这一帧的事件里挑出「主键」。
///
/// 两件必须做的事：
/// 1. **跳过修饰键自己**。egui 0.35 会把左右 Ctrl / Win **也**作为按键事件发出来
///    （见 egui-winit 的 `KeyCode::ControlLeft => Key::ControlLeft`），而修饰键不是
///    主键 —— 它们由钩子统一记录（`hotkey::current_mods`）。要是当成主键送进
///    `egui_key_to_vk`，会被判成"不支持"：主人**刚按下 Ctrl 的瞬间**捕捉就报错、
///    还把半截状态清空，于是 Ctrl+Win 永远录不出来。
/// 2. 只认按下（`pressed: true`），松开的事件不要。
fn take_main_key(events: &[egui::Event]) -> Option<egui::Key> {
    events.iter().find_map(|e| match e {
        egui::Event::Key {
            key, pressed: true, ..
        } if !hotkey::is_modifier_key(*key) => Some(*key),
        _ => None,
    })
}

/// 项目地址（「项目地址」按钮打开的那个链接）
const PROJECT_URL: &str = "https://github.com/HaiSeaman/BBVoxi";

/// 用系统默认浏览器打开项目地址。
///
/// **不能用 `egui::Context::open_url` 或 `egui::Hyperlink`**：那两个只是把
/// `OutputCommand::OpenUrl` 投出去，而 eframe 0.35 只在 web runner 里消费它，
/// 原生窗口上没有任何人处理 —— 按下去会毫无反应（不报错，也不打开）。
///
/// 用 `ShellExecuteW` 是 Windows 上打开 URL 的标准做法：不需要经过 shell 解析，
/// 不会闪黑窗，也不需要额外的依赖。
#[cfg(windows)]
fn open_project_page() -> Result<(), String> {
    open_url(PROJECT_URL)
}

/// 打开任意网址（更新失败时的「手动下载」也走这里，逻辑一份就够）
#[cfg(windows)]
fn open_url(url: &str) -> Result<(), String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // 返回值声明成 HINSTANCE，但语义是错误码：> 32 才算成功（微软文档的经典坑）
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("系统拒绝了打开浏览器的请求（错误码 {code}）"))
    }
}

#[cfg(not(windows))]
fn open_project_page() -> Result<(), String> {
    open_url(PROJECT_URL)
}

#[cfg(not(windows))]
fn open_url(_url: &str) -> Result<(), String> {
    Err("仅支持 Windows".into())
}

/// 主题三选一的选项表（顺序即显示顺序）
const THEME_MODES: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];

fn theme_label(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::System => "跟随系统",
        ThemeMode::Light => "浅色",
        ThemeMode::Dark => "深色",
    }
}

/// 主题即时落盘用的配置副本：**上一次保存的配置 + 新主题**。
///
/// 抽成纯函数是为了把"落盘的东西不带编辑中的半成品"这件事钉进单测：
/// 主人改了一半的 API Key、没校验过的快捷键，都不该跟着主题一起写进文件。
fn theme_disk_copy(saved: &Config, mode: ThemeMode) -> Config {
    let mut disk = saved.clone();
    disk.theme = mode;
    disk
}

/// 分段选择器：几个互斥的选项排一排，选中项 = accent 胶囊底 + accent 字。
///
/// 页签（`tabs_bar`）、服务商（`provider_selector`）、主题（`theme_row`）
/// 共用这一个控件，视觉语言全窗口统一。返回本次点中的选项（没点返回 None），
/// "点了之后做什么"（立即生效 or 只改内存）由调用方决定。
///
/// `base` 是控件所在底色：选中底 = accent × `PILL_ALPHA` 叠在它上面，
/// 传错底色会让对比度校验（`palette_contrast_meets_wcag`）量不到真实值。
fn segmented<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    base: Color32,
    current: &mut T,
    items: &[T],
    label_of: impl Fn(T) -> &'static str,
    fill_width: bool,
) -> Option<T> {
    let mut picked = None;
    ui.horizontal(|ui| {
        let count = items.len() as f32;
        let gap = 6.0;
        // egui 在水平布局里给相邻 widget 自动加 item_spacing.x，加上显式
        // add_space(gap)，每两段之间实际隔 gap + item_spacing.x —— 等分公式
        // 里两笔都要扣，否则三段总宽比可用宽多出 2×item_spacing（实测
        // 选择器被撑出卡片、页签右沿画出窗口）。
        let seg_gap = gap + ui.style().spacing.item_spacing.x;
        // 等宽模式（页签、服务商）：铺满整行；行内模式（主题）：按固定宽
        let w = if fill_width {
            (ui.available_width() - (count - 1.0) * seg_gap) / count
        } else {
            80.0
        };
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                ui.add_space(gap);
            }
            let selected = *current == *item;
            let (fill, text_color, stroke) = if selected {
                (
                    // 用 `tint_over` 而不是就地写一个透明度：那个常量是被
                    // `palette_contrast_meets_wcag` 逐个校验过的；就地写死一个
                    // 数字，会让"真校验"漏掉真正画出来的那一对（评审抓到过一次）。
                    tint_over(base, p.accent),
                    p.accent,
                    Stroke::new(1.5, p.accent),
                )
            } else {
                (p.field, p.muted, Stroke::new(1.0, p.border))
            };
            let label = button_text(label_of(*item)).strong().color(text_color);
            // 用 `add_sized` 钉死每段恰好占 w 宽：直接 `ui.add(Button…min_size)`
            // 会把 Button 自带边距也加到段宽上（实测三段总宽比可用宽多 2×8px，
            // 页签行右沿直接画出窗口），等分就失准了。
            let response = ui.add_sized(
                [w, 32.0],
                egui::Button::new(label)
                    .fill(fill)
                    .stroke(stroke)
                    .corner_radius(CornerRadius::same(8)),
            );
            if response.clicked() {
                *current = *item;
                picked = Some(*item);
            }
        }
    });
    picked
}

/// 服务商分段选择器：三家一眼看全，比下拉少一次点击
fn provider_selector(ui: &mut egui::Ui, p: &Palette, current: &mut Provider) {
    segmented(
        ui,
        p,
        p.card,
        current,
        &Provider::ALL,
        short_label,
        true,
    );
}

fn short_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Qwen => "千问",
        Provider::Doubao => "豆包",
        Provider::Tencent => "腾讯云",
    }
}

fn icon_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("bbvoxi_icon_texture");
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([128, 128], ICON_WINDOW);
    let tex = ctx.load_texture("bbvoxi_icon", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

fn clock(d: std::time::Duration) -> String {
    format!("{:02}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

// 旧的本地 `egui_key_to_vk` 已删除：它和 hotkey.rs 各维护一份键表，
// 迟早对不上（分号就是这么坏的）。现在统一走 `hotkey::egui_key_to_vk`。

/// 测试用的小工具：造一个能离线跑起来的设置界面实例（不开窗、不联网、不写注册表）
#[cfg(test)]
mod test_util {
    use super::*;

    /// `prepare` 用来预置共享状态（实时文字、上次结果、提示……）。
    pub fn app(cfg: Config, prepare: impl FnOnce(&Shared)) -> SettingsApp {
        let ctx = egui::Context::default();
        let shared_cfg = Arc::new(Mutex::new(cfg.clone()));
        let shared = Arc::new(Shared::new(ctx));
        prepare(&shared);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SettingsApp::new(cfg, shared_cfg, shared, tx, Arc::new(AtomicBool::new(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态消息必须会自己退场。
    ///
    /// 回归（报告第 3 条）：`status` 只有写入、**没有任何清除点**，一条「已保存」
    /// 会永远挂在界面上，还把卡片越顶越高。
    #[test]
    fn status_messages_expire_after_ten_seconds() {
        let t0 = Instant::now();
        assert!(status_is_fresh(t0, t0), "刚写上去的必须看得见");
        assert!(
            status_is_fresh(t0, t0 + STATUS_TTL - Duration::from_millis(1)),
            "没到点不许提前消失"
        );
        assert!(
            !status_is_fresh(t0, t0 + STATUS_TTL),
            "到点就该退场（边界取 <）"
        );
        assert!(
            !status_is_fresh(t0, t0 + STATUS_TTL * 3),
            "过期很久的不能复活"
        );
    }

    /// 关窗前的判断：改了没保存必须被发现，改回原样就不算改动 ——
    /// 判错了两边都疼（回归：点 ✕ 直接隐藏，改了半天全没了）。
    #[test]
    fn unsaved_edits_are_noticed_before_closing() {
        let mut app = test_util::app(Config::default(), |_| {});
        assert!(
            !app.has_unsaved_changes(),
            "刚打开时不该说人家有没保存的改动"
        );
        app.edit.qwen.api_key = "sk-new".into();
        assert!(app.has_unsaved_changes(), "改了 API Key 却报'没有改动'");
        // 改回原样：不该再拦着主人关窗（比的是整份配置，不挑字段）
        app.edit = app.saved.clone();
        assert!(!app.has_unsaved_changes(), "改回原样就不算改动");
        // 再改一次，再走"保存成功"那条路（`save` 里会更新 `saved`）
        app.edit.options.smooth = !app.saved.options.smooth;
        assert!(app.has_unsaved_changes(), "改了开关必须被发现");
        app.saved = app.edit.clone();
        assert!(!app.has_unsaved_changes());
    }

    /// 回归（「不保存」其实一样都没丢）：选「不保存」必须**真的**把改动丢掉。
    ///
    /// 设置窗实例整个进程只建一次（`main` 里 new 一次，之后窗口只是 hide/show），
    /// 旧实现只清对话框标志、`edit` 一个字都没回滚 —— 下次打开设置窗，被丢弃的
    /// 改动原样回来（"未保存"小圆点还亮着），主人再改一处一点「保存」，那半截
    /// API Key 就一起落盘了。跟对话框自己写的"直接关掉的话，这次改的内容就没了"
    /// 正好相反。
    #[test]
    fn discarding_changes_really_rolls_them_back() {
        let mut app = test_util::app(Config::default(), |_| {});
        app.edit.qwen.api_key = "sk-填了一半".into();
        app.edit.options.smooth = !app.saved.options.smooth;
        assert!(app.has_unsaved_changes(), "改了东西却报没改");

        app.discard_changes();

        assert_eq!(app.edit, app.saved, "丢弃之后 edit 必须回到上一次保存的样子");
        assert!(
            !app.has_unsaved_changes(),
            "丢弃之后不该还留着「未保存的改动」"
        );
        assert!(
            app.edit.qwen.api_key.is_empty(),
            "填了一半的 API Key 被留下来了，下次一点保存就会写进文件"
        );
    }

    /// 下载进度换算：总大小未知时不能除零、不能显示假百分比，进度还不能超过 100%
    #[test]
    fn download_progress_never_divides_by_zero() {
        assert_eq!(download_progress(1024 * 1024, 0), "1.0 MB");
        assert_eq!(
            download_progress(4 * 1024 * 1024, 8 * 1024 * 1024),
            "50%（4.0/8.0 MB）"
        );
        assert!(
            download_progress(9 * 1024 * 1024, 8 * 1024 * 1024).contains("100%"),
            "进度超过总量时要夹到 100%，不能显示 112% 这种鬼数字"
        );
    }

    /// 更新状态机：后台消息进来之后底部那行该显示什么、失败该用什么颜色。
    ///
    /// 这条链是主人**唯一**能看见更新进展的地方，显示错了就等于更新过程是黑盒。
    #[test]
    fn update_states_drive_the_footer_line() {
        let p = Palette::of(false);
        let mut app = test_util::app(Config::default(), |_| {});
        assert!(
            app.update_line(&p).is_none(),
            "没在更新时不该占用底部那一行"
        );

        // 1) 查到新版本
        app.apply_update_msg(UpdateMsg::Found(Box::new(Release {
            version: "1.5.0".into(),
            notes: "修了几个毛病".into(),
            asset_name: "BBVoxi-1.5.0.exe".into(),
            download_url: "https://example.invalid/1.5.0.exe".into(),
            digest: Some("sha256:abcdef".into()),
        })));
        assert!(matches!(app.update, UpdateState::Found(_)), "应当停在等确认");
        let (line, _) = app.update_line(&p).expect("发现新版本时底部要有字");
        assert!(line.contains("1.5.0"), "要写清是哪个版本：{line}");

        // 2) 下载中（真实顺序：确认之后先置成下载中，再收进度）
        app.update = UpdateState::Downloading {
            version: "1.5.0".into(),
            done: 0,
            total: 0,
        };
        app.apply_update_msg(UpdateMsg::Progress(4 * 1024 * 1024, 8 * 1024 * 1024));
        let (line, _) = app.update_line(&p).expect("下载中要有进度");
        assert!(line.contains("50%"), "进度要算对：{line}");

        // 3) 装好了：必须提示"要重启才生效"
        app.apply_update_msg(UpdateMsg::Installed("1.5.0".into()));
        let (line, _) = app.update_line(&p).expect("装好后要有提示");
        assert!(line.contains("重启"), "装好后要提示重启才生效：{line}");

        // 4) 失败：**不占用底部那行**（否则后面所有状态消息都被它压住），
        //    原因走状态消息如实说出来
        app.apply_update_msg(UpdateMsg::Failed("下载失败：等待超时".into()));
        assert!(
            app.update_line(&p).is_none(),
            "失败不该继续霸占底部那一行"
        );
        assert!(
            app.status.as_ref().is_some_and(|s| !s.ok && s.text.contains("更新失败")),
            "失败原因要进状态消息（主人可能只看底部那行）"
        );
    }

    /// 后台线程异常退出（panic/被杀）时通道会断开：界面必须把"更新中"收掉，
    /// 否则按钮永远灰着、主人以为程序卡死，只能重启 —— 而他明明什么都没做错。
    #[test]
    fn a_dead_update_thread_does_not_freeze_the_button() {
        let mut app = test_util::app(Config::default(), |_| {});
        let (tx, rx) = std::sync::mpsc::channel::<UpdateMsg>();
        app.update_rx = Some(rx);
        app.update = UpdateState::Checking;

        drop(tx); // 后台线程没了
        app.poll_update();

        assert!(
            matches!(app.update, UpdateState::Failed(_)),
            "通道断开要当成一次失败收场，不能一直显示「检查中」"
        );
        assert!(app.update_rx.is_none(), "断开之后要把接收端放掉");
    }

    /// 已经收到最终结果（比如"已是最新"）之后通道断开，**不能**再补一条失败：
    /// 那会把"已是最新版本"这条正常提示顶掉，主人以为出错了。
    #[test]
    fn a_closed_channel_after_the_final_message_is_not_a_failure() {
        let mut app = test_util::app(Config::default(), |_| {});
        let (tx, rx) = std::sync::mpsc::channel::<UpdateMsg>();
        app.update_rx = Some(rx);
        app.update = UpdateState::Checking;
        tx.send(UpdateMsg::UpToDate("1.4.0".into())).unwrap();
        drop(tx);

        app.poll_update();

        assert_eq!(app.update, UpdateState::Idle, "已是最新就该回到空闲");
        assert!(
            app.status.as_ref().is_some_and(|s| s.ok),
            "该显示的是「已是最新」，不能变成失败提示"
        );
    }

    /// 回归（更新失败之后，底部反馈条被它永久霸占）：`update_line` 排在普通状态
    /// 消息**前面**，失败状态要是一直返回 `Some`，主人之后点「保存」「刷新麦克风」
    /// 得到的反馈全被压住，看到的一直是那句旧的"更新失败"。
    /// 失败原因本身会进状态消息（`apply_update_msg`），那条 10 秒后自己退场，
    /// 所以这里只要保证"不再霸屏"即可。
    #[test]
    fn a_failed_update_does_not_hide_later_status_messages() {
        let p = Palette::of(false);
        let mut app = test_util::app(Config::default(), |_| {});
        app.apply_update_msg(UpdateMsg::Failed("下载失败：等待超时".into()));
        assert!(
            app.update_line(&p).is_none(),
            "失败之后不该再占用底部那一行（否则后面所有状态都看不见）"
        );
        assert!(
            app.status.as_ref().is_some_and(|s| !s.ok),
            "失败原因要用状态消息说出来（那条会自然退场）"
        );
    }

    /// 关窗确认的状态机：默认不弹 → 收到请求才弹 → 隐藏请求取走即清零
    #[test]
    fn close_confirmation_is_only_shown_when_requested() {
        let mut app = test_util::app(Config::default(), |_| {});
        assert!(!app.confirm_close, "默认不该弹确认框");
        assert!(!app.take_hide_request(), "默认没有隐藏请求");
        app.ask_before_close();
        assert!(app.confirm_close, "被请求后要弹出来");
        app.hide_requested = true;
        assert!(app.take_hide_request(), "隐藏请求要能被取到");
        assert!(!app.take_hide_request(), "取走即清零，不能重复隐藏");
    }

    /// 真的去开一次浏览器，验证 `ShellExecuteW` 这条链路能通。
    ///
    /// 标了 `#[ignore]`：它会**真的用你的默认浏览器打开项目页**，所以不进
    /// `cargo test` 的默认集合。需要烟测时手动跑：
    /// `cargo test open_project_page -- --ignored`
    ///
    /// 为什么值得有这么一条：这个功能**没有任何可在单测里断言的东西** ——
    /// 按钮画对了、函数编过了，都不代表浏览器真的会起来（egui 那套
    /// `open_url` 就是典型反例：编译通过、运行不报错，只是什么都不发生）。
    #[test]
    #[ignore = "会真的打开浏览器；需要时手动跑：cargo test open_project_page -- --ignored"]
    fn open_project_page_reports_success() {
        assert_eq!(PROJECT_URL, "https://github.com/HaiSeaman/BBVoxi");
        open_project_page().expect("ShellExecuteW 应该能打开项目地址");
    }

    /// 改键捕捉必须跟着界面一起过期。
    ///
    /// 判错的代价不对称：漏判 → `paused` 永远是 true，**全局快捷键彻底失效**
    /// 且没有任何提示；误判 → 主人重新点一次「重新录制」。所以这里要求
    /// "超过容忍度"才算失效（正好等于不算），并且没在捕捉时一律不动作。
    #[test]
    fn capture_expires_only_when_capturing_and_past_the_limit() {
        assert!(
            !capture_is_stale(false, CAPTURE_IDLE_LIMIT * 10),
            "没在捕捉时谈不上失效，不能去动 paused"
        );
        assert!(
            !capture_is_stale(true, Duration::from_millis(100)),
            "界面刚画过（logic 100ms 一帧），绝不能误杀"
        );
        assert!(
            !capture_is_stale(true, CAPTURE_IDLE_LIMIT),
            "刚好到容忍度不算失效（边界取 >）"
        );
        assert!(
            capture_is_stale(true, CAPTURE_IDLE_LIMIT + Duration::from_millis(1)),
            "超过容忍度必须收掉，否则全局快捷键会永久失效"
        );
    }

    /// 主键映射必须覆盖主人可能按的各种键，且映射表要认得方向键和符号键
    /// （旧版本这里只有字母数字和 F1~F12，主人按方向键会被判成"不支持"）。
    #[test]
    fn key_mapping_covers_letters_digits_and_function_keys() {
        use hotkey::egui_key_to_vk;
        assert_eq!(egui_key_to_vk(egui::Key::A), Some(0x41));
        assert_eq!(egui_key_to_vk(egui::Key::Num1), Some(0x31));
        assert_eq!(egui_key_to_vk(egui::Key::F9), Some(0x78));
        assert_eq!(egui_key_to_vk(egui::Key::Space), Some(0x20));
        assert_eq!(egui_key_to_vk(egui::Key::Backtick), Some(0xC0));
        assert_eq!(egui_key_to_vk(egui::Key::ArrowDown), Some(0x28));
        assert_eq!(egui_key_to_vk(egui::Key::PageUp), Some(0x21));
        assert_eq!(egui_key_to_vk(egui::Key::Semicolon), Some(0xBA));
        // 修饰键自己不是主键（它由钩子记录），映射表里不该有它们
        assert_eq!(egui_key_to_vk(egui::Key::ControlLeft), None);
        assert_eq!(egui_key_to_vk(egui::Key::SuperLeft), None);
    }

    fn key_event(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// 从事件里挑主键时必须**跳过修饰键自己**。
    ///
    /// 这是 Ctrl+Win 录不上的直接原因：egui 0.35 会把左右 Ctrl / Win 也作为
    /// 按键事件发出来，旧代码把它们当主键送进映射表 → 判成"不支持" → 捕捉当场
    /// 报错并清空半截状态。主人看到的是"我按了 Ctrl，界面就红字报错"。
    #[test]
    fn take_main_key_skips_modifier_keys() {
        // 只按了 Ctrl：修饰键事件必须被跳过，不能当成主键
        assert_eq!(
            take_main_key(&[key_event(egui::Key::ControlLeft, true)]),
            None
        );
        assert_eq!(
            take_main_key(&[key_event(egui::Key::SuperLeft, true)]),
            None
        );
        // Ctrl + A：Ctrl 被跳过，A 就是主键
        assert_eq!(
            take_main_key(&[
                key_event(egui::Key::ControlLeft, true),
                key_event(egui::Key::A, true)
            ]),
            Some(egui::Key::A)
        );
        // 松开的事件不算（pressed: false）
        assert_eq!(take_main_key(&[key_event(egui::Key::A, false)]), None);
    }

    /// Ctrl+Win 必须能录上，**不管哪根手指先松**。
    ///
    /// 旧代码每帧用"此刻按住的修饰键"覆盖记忆：手指一根根松开，最后留下的只是
    /// 最后松的那根（Ctrl+Win 变成"只剩 Win"），校验器报"纯修饰键组合至少要两个
    /// 修饰键"。主人折腾半天也存不进去。这里把两种松手顺序都钉死。
    #[test]
    fn capture_step_keeps_ctrl_win_regardless_of_release_order() {
        // 复用 `CTRL_WIN` 常量，同一份数据不写两遍（写两遍就会有一天只改一处、
        // 测试还全绿）；期望值直接用 `Hotkey::default()` —— 录出来的必须正好是
        // 出厂默认值，这一条同时把「默认 = Ctrl+Win」钉死在测试里。
        let ctrl_only = hotkey::Mods {
            win: false,
            ..CTRL_WIN
        };
        let win_only = hotkey::Mods {
            ctrl: false,
            ..CTRL_WIN
        };

        // 顺序一：先按 Ctrl → 再按 Win → 松开 Ctrl → 再松开 Win
        let mut pending = None;
        assert_eq!(capture_step(ctrl_only, None, &mut pending), None);
        assert_eq!(capture_step(CTRL_WIN, None, &mut pending), None);
        assert_eq!(
            capture_step(win_only, None, &mut pending),
            None,
            "先松 Ctrl 不能把记忆缩小"
        );
        assert_eq!(
            capture_step(hotkey::Mods::default(), None, &mut pending),
            Some(Hotkey::default()),
            "全部松开时必须录到 Ctrl+Win，而不是「只剩 Win」"
        );

        // 顺序二：先按 Ctrl → 再按 Win → 松开 Win → 再松开 Ctrl
        let mut pending = None;
        assert_eq!(capture_step(ctrl_only, None, &mut pending), None);
        assert_eq!(capture_step(CTRL_WIN, None, &mut pending), None);
        assert_eq!(
            capture_step(ctrl_only, None, &mut pending),
            None,
            "先松 Win 同样不能缩小记忆"
        );
        assert_eq!(
            capture_step(hotkey::Mods::default(), None, &mut pending),
            Some(Hotkey::default()),
            "两种松手顺序都必须录到 Ctrl+Win"
        );
    }

    const CTRL_WIN: hotkey::Mods = hotkey::Mods {
        ctrl: true,
        alt: false,
        shift: false,
        win: true,
    };

    /// 有主键时立刻成组合，修饰键（含 Win）原样带过去。
    ///
    /// 这是 Ctrl+Win 能被录下来的关键：Win 不在 egui 的 `Modifiers` 里，
    /// 只能从钩子读 —— 这个用例把"读钩子"这件事的结果钉死。
    #[test]
    fn capture_with_main_key_combines_hook_modifiers() {
        let mut pending = None;
        let got = capture_step(CTRL_WIN, Some(0x31), &mut pending).expect("按下主键就该成组合");
        assert_eq!(
            got,
            Hotkey {
                ctrl: true,
                alt: false,
                shift: false,
                win: true,
                vk: 0x31,
            }
        );
        assert_eq!(hotkey::parse(&got.to_config()).unwrap(), got);
        assert!(pending.is_none(), "已经录完了，不该再留半截状态");
    }

    /// 纯修饰键组合：手指全松开的那一刻才算录完。
    #[test]
    fn capture_commits_pure_modifier_combo_on_release() {
        let mut pending = None;
        assert_eq!(
            capture_step(CTRL_WIN, None, &mut pending),
            None,
            "还按着不放，不能提前定案（也许主人还要补个主键）"
        );
        assert_eq!(pending, Some(CTRL_WIN), "要先记住凑齐了什么");

        let got =
            capture_step(hotkey::Mods::default(), None, &mut pending).expect("全部松手就该成组合");
        assert_eq!(got.to_config(), "ctrl+win");
        assert_eq!(got.display(), "Ctrl + Win");
        assert!(pending.is_none(), "定案后必须清空，否则下一帧会再报一次");
        assert_eq!(
            capture_step(hotkey::Mods::default(), None, &mut pending),
            None,
            "清空之后不能重复触发"
        );
    }

    /// 只按一个修饰键 → 不合法，由 `commit_hotkey` 里的校验器挡下来。
    /// 这里守住"录到的候选确实会被挡"这件事，否则会悄悄存下一个没法用的快捷键。
    #[test]
    fn capture_rejects_single_modifier() {
        let mut pending = None;
        let ctrl_only = hotkey::Mods {
            ctrl: true,
            ..hotkey::Mods::default()
        };
        assert_eq!(capture_step(ctrl_only, None, &mut pending), None);
        let got = capture_step(hotkey::Mods::default(), None, &mut pending).expect("松手后成候选");
        assert!(
            hotkey::parse(&got.to_config()).is_err(),
            "只按一个修饰键必须被挡下来"
        );
    }

    /// 空手点一下「重新录制」又原样松开，什么都不该录到。
    #[test]
    fn capture_without_any_key_records_nothing() {
        let mut pending = None;
        assert_eq!(
            capture_step(hotkey::Mods::default(), None, &mut pending),
            None
        );
        assert!(pending.is_none());
    }

    #[test]
    fn clock_formats_minutes() {
        assert_eq!(clock(std::time::Duration::from_secs(65)), "01:05");
    }

    /// 对比度必须是**真**校验 —— 旧用例只比较 R 通道差值（>100 / >60），
    /// 实测出来的 2.88、3.37 一个都抓不到，测试照样全绿（报告点名的"假安全网"）。
    ///
    /// 这里按 WCAG 2.x 相对亮度公式，把界面上真实出现的每一对"前景 / 底色"
    /// 逐对钉住，门槛取正文标准 4.5:1（12~13px 的字都算正文）。
    #[test]
    fn palette_contrast_meets_wcag() {
        fn hex(c: Color32) -> String {
            let [r, g, b, _] = c.to_array();
            format!("#{r:02X}{g:02X}{b:02X}")
        }
        let mut bad: Vec<String> = Vec::new();
        let mut check = |dark: bool, what: &str, fg: Color32, bg: Color32| {
            let r = contrast_ratio(fg, bg);
            if r < 4.5 {
                bad.push(format!(
                    "{what}（dark={dark}）只有 {r:.2}:1（要求 4.5）：{} 叠在 {} 上",
                    hex(fg),
                    hex(bg)
                ));
            }
        };
        for dark in [false, true] {
            let p = Palette::of(dark);
            let (card, field, bg) = (p.card, p.field, p.bg);

            // 正文与次要文字：卡片、输入框、窗口底色上都可能出现
            for surface in [card, field, bg] {
                check(dark, "正文", p.text, surface);
                check(dark, "次要文字", p.muted, surface);
            }
            // 状态色：提示行、错误行、提醒行都是直接写在卡片上的小字
            for surface in [card, bg] {
                check(dark, "成功色", p.ok, surface);
                check(dark, "警告色", p.warn, surface);
                check(dark, "危险色", p.danger, surface);
            }
            // 状态胶囊：底色 = 状态色 × PILL_ALPHA 叠在底上，文字就是状态色。
            // 胶囊画在页面底色上、提示行画在卡片上，两处的底都要算。
            for (name, color) in [("成功", p.ok), ("警告", p.warn), ("危险", p.danger)] {
                for surface in [card, bg] {
                    check(
                        dark,
                        &format!("胶囊上的{name}字"),
                        color,
                        tint_over(surface, color),
                    );
                }
            }
            // 主按钮与分段选择器
            check(dark, "主按钮文字", p.on_accent, p.accent);
            check(dark, "选中的服务商文字", p.accent, tint_over(card, p.accent));
            // 禁用控件：字是"正文色 × DISABLED_ALPHA"画上去的
            check(
                dark,
                "被禁用的控件文字",
                blend_over(card, p.text, DISABLED_ALPHA),
                card,
            );
            check(
                dark,
                "被禁用的控件文字（输入框上）",
                blend_over(field, p.text, DISABLED_ALPHA),
                field,
            );
        }
        assert!(
            bad.is_empty(),
            "有 {} 处配色读不清：\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// 控件文字色在三个交互态之间不许跳变，默认文字必须是正文色。
    ///
    /// 回归 1：以前 inactive = muted、hovered/active = text，三态不同色 ——
    /// 鼠标一划过去，复选框文字就从灰变黑（报告第 6 条）。
    /// 回归 2：`noninteractive.fg_stroke` 干脆没设，没显式取色的标签用 egui 默认
    /// 灰(140)：深色主题下比 muted 还淡，层级颠倒（报告第 5 条）。
    #[test]
    fn widget_text_colors_are_consistent_and_dark_enough() {
        for dark in [false, true] {
            let p = Palette::of(dark);
            let ctx = egui::Context::default();
            let mut got = None;
            let _ = ctx.run_ui(Default::default(), |ui| {
                apply_widget_style(ui, &p);
                let v = ui.visuals();
                got = Some((
                    v.widgets.inactive.fg_stroke.color,
                    v.widgets.hovered.fg_stroke.color,
                    v.widgets.active.fg_stroke.color,
                    v.widgets.noninteractive.fg_stroke.color,
                    v.disabled_alpha,
                ));
            });
            let (inactive, hovered, active, noninteractive, alpha) = got.expect("跑过一帧");
            assert_eq!(inactive, p.text, "没悬停时控件文字要用正文色（dark={dark}）");
            assert_eq!(hovered, p.text, "鼠标划过时文字不许变色（dark={dark}）");
            assert_eq!(active, p.text, "按下时文字不许变色（dark={dark}）");
            assert_eq!(
                noninteractive, p.text,
                "普通标签要用正文色，不能是 egui 默认灰（dark={dark}）"
            );
            assert!(
                alpha >= 0.6,
                "禁用透明度 {alpha} 太低，会把被禁用的说明压到读不清（dark={dark}）"
            );
        }
    }

    /// 状态胶囊的判定：**"照例留了一份到剪贴板"不算异常**，不该常亮警告色。
    ///
    /// 回归（评审发现）：`kept_on_clipboard` 在每次识别成功时都会置真（「识别结果总留
    /// 一份到剪贴板」默认开着），旧判据把它当成"结果改走剪贴板了"来报 —— 于是识别成功
    /// 过一次之后，胶囊永远停在琥珀色的提醒态、"就绪"再也不出现，而托盘那边并不报警，
    /// 两处说法不一致。只有 `notice`（这次真没能自动输入）才是"请看一眼"。
    #[test]
    fn a_routine_clipboard_copy_does_not_turn_the_pill_into_a_warning() {
        let p = Palette::of(false);
        let mut snap = Snapshot {
            kept_on_clipboard: true, // 常态：结果照例留了一份
            last_result: "你好".into(),
            ..Snapshot::default()
        };
        assert_eq!(
            pill_state(&snap, false, true, &p),
            (p.ok, "就绪"),
            "照例留一份不是异常，胶囊该显示就绪"
        );
        snap.notice = Some("录音途中切换了窗口，结果改放进剪贴板".into());
        assert_eq!(
            pill_state(&snap, false, true, &p),
            (p.warn, "有提示"),
            "真正需要看一眼的时候才亮提醒色"
        );
        snap.error = Some("网络不通".into());
        assert_eq!(pill_state(&snap, false, true, &p), (p.danger, "出错了"));
        snap.error = None;
        snap.recording = true;
        assert_eq!(pill_state(&snap, false, true, &p), (p.danger, "录音中"));
    }

    /// 「保存并关闭」失败时必须留下一条看得见的原因（对话框会把它显示在自己身上），
    /// 而且**改动仍然算"没保存"** —— 绝不能悄悄把主人的改动丢掉。
    #[test]
    fn a_failed_save_keeps_a_reason_and_the_changes() {
        let mut app = test_util::app(Config::default(), |_| {});
        app.edit.hotkey = "只按一个键不行".into(); // 非法快捷键 → 保存必然失败
        assert!(!app.save(), "非法快捷键不该保存成功");
        let status = app.status.as_ref().expect("失败必须留下一条状态消息");
        assert!(!status.ok, "这条消息必须是失败态");
        assert!(
            app.has_unsaved_changes(),
            "保存失败后改动仍算没保存，关窗前还得拦一次"
        );
    }

    /// 深色主题下"正文"必须比"次要文字"更深（教材级的层级要求）。
    ///
    /// 回归：`apply_widget_style` 以前没覆盖 `noninteractive.fg_stroke`，于是没显式
    /// 取色的标签用的是 egui 默认灰(140) —— 深色主题下正文（5.12:1）反而比程序自己的
    /// muted 色（6.68:1）**更淡**，本该最深的层级最浅。颜色对不对由
    /// `palette_contrast` 一组用例按 WCAG 公式逐对把关，这条只管"谁更深"。
    #[test]
    fn dark_text_is_darker_than_muted_text() {
        let p = Palette::of(true);
        assert!(
            crate::ui::contrast_ratio(p.text, p.card) > crate::ui::contrast_ratio(p.muted, p.card),
            "深色主题里正文必须比次要文字更清楚（以前这里是反的）"
        );
    }

    /// 主题即时落盘的副本：**只带主题，不带编辑中的半成品**。
    ///
    /// 主人改了一半的 API Key、没校验过的快捷键都不该跟着主题一起写进文件
    /// （否则快捷键格式错了也会被悄悄存进去）。落盘失败的场景不在这里测
    /// ——写文件本身由 `config` 模块的原子写测试兜底。
    #[test]
    fn theme_disk_copy_carries_only_the_theme() {
        let mut saved = Config::default();
        saved.qwen.api_key = "sk-saved".into();
        let mut editing = saved.clone();
        editing.qwen.api_key = "sk-half-typed".into(); // 还没保存的半成品
        let disk = theme_disk_copy(&saved, ThemeMode::Dark);
        assert_eq!(disk.theme, ThemeMode::Dark, "新主题要写进文件");
        assert_eq!(
            disk.qwen.api_key, "sk-saved",
            "落盘的是上次保存的值，不是编辑中的半成品"
        );
        assert_ne!(
            disk, editing,
            "编辑中的改动不能被主题切换顺手带走（edit 与 disk 必须不同）"
        );
    }

    /// 主题切换后 `has_unsaved_changes` 的判定不受影响：主题即时落盘时
    /// `saved` 也同步更新，其他字段的"改了没保存"照常被发现。
    #[test]
    fn theme_switch_does_not_swallow_other_unsaved_changes() {
        let mut app = test_util::app(Config::default(), |_| {});
        app.edit.qwen.api_key = "sk-new".into(); // 改了没保存
        assert!(app.has_unsaved_changes());
        // 模拟 apply_theme_now 的内存部分（不写文件）
        let disk = theme_disk_copy(&app.saved, ThemeMode::Dark);
        app.edit.theme = ThemeMode::Dark;
        app.saved = disk;
        assert!(
            app.has_unsaved_changes(),
            "主题即时保存了，但 API Key 的改动必须仍然算「没保存」"
        );
    }
}

/// 无窗口布局实测。
///
/// 规矩：本工程**不开窗做界面测试**（慢、不稳、还量不到坐标）。这里跑的是同一份
/// `SettingsApp::ui`，只是把 egui 真实的绘制指令抓下来量尺寸 —— 报告里的
/// "内容总高 1274px""「最近识别」起点 599px"就是这么量的；改版之后用同一把尺子复核。
#[cfg(all(test, windows))]
mod layout {
    use super::*;
    use egui::{Pos2, Rect, Vec2};

    /// 一帧里画出来的一段文字
    #[derive(Debug, Clone)]
    pub struct Text {
        pub s: String,
        pub x: f32,
        pub y: f32,
        pub w: f32,
        pub h: f32,
    }

    /// 一帧里画出来的一个矩形（只留布局测试用得到的几何与填充色）
    #[derive(Debug, Clone)]
    pub struct RectI {
        pub y: f32,
        pub w: f32,
        pub h: f32,
        pub fill: [u8; 4],
    }

    pub struct Snapshot {
        pub w: f32,
        pub h: f32,
        pub texts: Vec<Text>,
        pub rects: Vec<RectI>,
    }

    /// 面板底色是整屏宽的大矩形，不是"内容"，量内容时要把它排除。
    /// 窗口 580 宽（见 `main.rs::window_geometry`）→ 面板底色矩形 580、
    /// 中央卡片 548：取 560 正好把两者分开。
    const PANEL_MIN_W: f32 = 560.0;
    /// 底部区域（「最近识别」反馈条 + 状态行 + 按钮行）占窗口最底下这一块，
    /// 量"内容总高"时整块排除
    const BOTTOM_RESERVED: f32 = 140.0;

    impl Snapshot {
        fn sizes(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
            self.rects
                .iter()
                .filter(|r| r.w < PANEL_MIN_W && r.y < self.h - BOTTOM_RESERVED)
                .map(|r| (r.y, r.y + r.h))
                .chain(
                    self.texts
                        .iter()
                        .filter(|t| t.y < self.h - BOTTOM_RESERVED)
                        .map(|t| (t.y, t.y + t.h)),
                )
        }

        /// 内容总高：从内容最顶端到最底一个控件的下沿
        pub fn content_height(&self) -> f32 {
            let top = self.sizes().map(|(y, _)| y).fold(f32::INFINITY, f32::min);
            let bottom = self.sizes().map(|(_, b)| b).fold(0.0f32, f32::max);
            bottom - top
        }

        /// 某段文字的左上角 y
        pub fn text_y(&self, s: &str) -> Option<f32> {
            self.texts.iter().find(|t| t.s == s).map(|t| t.y)
        }

        /// 卡片数量：卡片 = 卡片底色 + 接近整宽（但不占满整宽）+ 有一定高度。
        /// 底部操作条也是卡片底色，但它占满整宽，用宽度上限排除。
        pub fn card_count(&self, card: [u8; 4]) -> usize {
            self.rects
                .iter()
                .filter(|r| {
                    r.fill == card && r.w > 400.0 && r.w < PANEL_MIN_W && r.h > 40.0
                })
                .count()
        }
    }

    fn walk(shape: &egui::Shape, snap: &mut Snapshot) {
        match shape {
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, snap)),
            egui::Shape::Rect(r) => snap.rects.push(RectI {
                y: r.rect.min.y,
                w: r.rect.width(),
                h: r.rect.height(),
                fill: r.fill.to_array(),
            }),
            egui::Shape::Text(t) => {
                let size = t.galley.size();
                snap.texts.push(Text {
                    s: t.galley.text().to_owned(),
                    x: t.pos.x,
                    y: t.pos.y,
                    w: size.x,
                    h: size.y,
                });
            }
            _ => {}
        }
    }

    /// 中文字体是量中文界面的前提。没有字体就量不准（豆腐块的宽度不一样），
    /// 所以这种情况**直接报错**，不许"跳过 = 通过"（那正是报告里说的假安全网）。
    fn require_cjk_font() {
        const CANDIDATES: [&str; 2] = [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simhei.ttf",
        ];
        assert!(
            CANDIDATES.iter().any(|p| std::path::Path::new(p).exists()),
            "布局测试需要系统中文字体（{}），否则量出来的中文宽度不可信",
            CANDIDATES.join(" / ")
        );
    }

    /// 跑一帧真实布局（浅色主题 + 真实中文字体，与正式运行同一套 `ui()`）。
    ///
    /// 视口故意给得很高（1500）：滚动区一旦被截断，量到的"总高"就只是可视高度。
    pub fn probe(cfg: Config, w: f32, h: f32, prepare: impl FnOnce(&Shared)) -> Snapshot {
        probe_tab(cfg, w, h, prepare, Tab::Service)
    }

    /// 同 `probe`，但可以指定停在哪个页签（页签内容不同，各自都要量）。
    pub fn probe_tab(
        cfg: Config,
        w: f32,
        h: f32,
        prepare: impl FnOnce(&Shared),
        tab: Tab,
    ) -> Snapshot {
        probe_with(cfg, w, h, prepare, tab, |_| {})
    }

    /// 同 `probe_tab`，另外允许在渲染前直接动界面实例本身
    /// （例如塞一台名字特别长的麦克风，量"名字长了会不会把这一行撑破"）。
    pub fn probe_with(
        cfg: Config,
        w: f32,
        h: f32,
        prepare: impl FnOnce(&Shared),
        tab: Tab,
        tune: impl FnOnce(&mut SettingsApp),
    ) -> Snapshot {
        require_cjk_font();
        let ctx = egui::Context::default();
        crate::setup_cjk_font(&ctx);
        setup_style(&ctx);
        ctx.set_theme(egui::ThemePreference::Light);
        let mut app = super::test_util::app(cfg, prepare);
        app.tab = tab;
        tune(&mut app);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| app.ui(ui));
        let mut snap = Snapshot {
            w,
            h,
            texts: Vec::new(),
            rects: Vec::new(),
        };
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut snap);
        }
        snap
    }

    /// 有凭据、有结果时的样子（配置齐全的日常状态）
    fn full_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.qwen.api_key = "sk-test".into();
        cfg
    }

    /// 验收线 1：**「最近识别」反馈条钉在底部**。
    ///
    /// 它现在是底部固定区（反馈条 + 按钮行）的一部分，不随页签切换、不参与
    /// 滚动：无论停在哪一页、无论内容多长，测试识别的结果一定看得见。
    /// 旧版它排在配置卡后面，768 高的笔记本上整块被挤出屏幕外（报告第 1 条）。
    #[test]
    fn result_strip_is_pinned_to_the_bottom() {
        let snap = probe(Config::default(), 580.0, 1500.0, |_| {});
        let top = snap
            .text_y("最近识别")
            .expect("界面上必须有「最近识别」这一区");
        assert!(
            top >= snap.h - BOTTOM_RESERVED,
            "「最近识别」起点 {top}px，应在底部固定区（窗口高 1500，底部区从 {} 起）",
            snap.h - BOTTOM_RESERVED
        );
        assert!(
            top <= snap.h - 40.0,
            "「最近识别」起点 {top}px —— 贴得太低会被窗口下沿切掉"
        );
    }

    /// 任何一页、任何窗口宽度下，面板都不许被内容撑宽。
    ///
    /// 抓的是一类很容易复发的毛病：某一行里塞了一段很长的说明文字，布局被它顶宽 ——
    /// 卡片跟着变宽，右对齐的东西（版本号、状态胶囊）就被推出窗外。
    /// 实测复现过一次：把「剪贴板粘贴兜底」的说明从 27 字加长到 43 字，
    /// 面板就从 580 变成 613，底部版本号右沿跑到 589（窗口只有 580）。
    #[test]
    fn content_never_widens_the_panel_beyond_the_window() {
        for (w, h) in [(580.0, 1500.0), (520.0, 560.0)] {
            for tab in Tab::ALL {
                let snap = probe_tab(Config::default(), w, h, |_| {}, tab);
                let widest = snap.rects.iter().map(|r| r.w).fold(0.0f32, f32::max);
                assert!(
                    widest <= w + 0.5,
                    "{w}×{h} 的 {tab:?} 页里有 {widest:.1} 宽的控件，把面板撑宽了（窗口只有 {w}）"
                );
            }
        }
    }

    /// 验收线 2：每个页签一屏放得下（560 高的最小窗口不滚也能看到大部分内容），
    /// 卡片每页一张（识别选项页行数最多，是最紧的一页）。
    #[test]
    fn content_is_compact() {
        for tab in Tab::ALL {
            let snap = probe_tab(Config::default(), 580.0, 1500.0, |_| {}, tab);
            let h = snap.content_height();
            assert!(
                h <= 600.0,
                "{:?} 页内容总高 {h}px，超了（页签化后每页都该一屏放得下）",
                tab
            );
            assert_eq!(
                snap.card_count([255, 255, 255, 255]),
                1,
                "{:?} 页应有且仅有一张内容卡",
                tab
            );
        }
    }

    /// 通用页要有「麦克风」选择（需求：设置 → 通用里能选录音用哪台麦克风）。
    ///
    /// 为什么用布局快照守着它：这一行是加在「通用」页里，而该页的高度预算本来
    /// 就紧（`content_is_compact` 卡在 600px）；同时下拉栏的当前值必须**一眼看见**
    /// ——"跟随系统默认"和"用某一台具体设备"长得一样的话，主人根本不知道自己
    /// 现在在用哪个麦克风，也就没法判断该不该改。
    #[test]
    fn general_page_has_a_microphone_picker() {
        let snap = probe_tab(full_cfg(), 580.0, 1500.0, |_| {}, Tab::General);
        assert!(
            snap.text_y("麦克风").is_some(),
            "通用页没有「麦克风」这一行（主人要求加的位置就是这里）"
        );
        assert!(
            snap.text_y("系统默认").is_some(),
            "没选过时必须显示成「系统默认」，不能是空的或换成别的字样"
        );
        assert!(
            snap.text_y("刷新").is_some(),
            "插上/拔掉麦克风后要能自己刷新列表，不必重启程序"
        );
    }

    /// 选中的麦克风被拔掉之后：通用页照样一屏放得下，而且**提醒必须看得见**。
    ///
    /// 这是这个功能最容易糊弄过去的一档：设备不在了的时候，界面既要照常说清
    /// "录音会退回系统默认设备"，又不能把这一页顶出高度预算（见 `content_is_compact`），
    /// 还不能在下拉栏里装作"系统默认"（那和下面的提醒自相矛盾）。
    #[test]
    fn general_page_stays_compact_when_the_chosen_microphone_is_gone() {
        let mut cfg = full_cfg();
        cfg.options.mic_device =
            "wasapi:{0.0.0.00000000}.{deadbeef-0000-0000-0000-000000000000}".into();
        let snap = probe_tab(cfg, 580.0, 1500.0, |_| {}, Tab::General);
        assert!(
            snap.texts
                .iter()
                .any(|t| t.s.contains("录音会自动改用系统默认设备")),
            "选中的麦克风不在线时，必须写清楚录音会退回系统默认设备"
        );
        assert!(
            snap.texts.iter().any(|t| t.s.contains("不在线")),
            "下拉栏不能装作是「系统默认」，要写明已选的那台不在线"
        );
        let h = snap.content_height();
        assert!(h <= 600.0, "选了不在线的麦克风之后，通用页高 {h}px，超预算了");
    }

    /// 回归（设备名一长，「刷新」按钮就被顶出窗口、点都点不到）：
    /// egui 0.35 里 `ComboBox::width` 只是**最小宽度**（combo_box.rs：
    /// `actual_width = galley宽 + 图标 … .at_least(minimum_width)`），而横向布局里
    /// 按钮文字是 `Extend` 模式（不换行、不截断）—— 设备名有多宽，按钮就有多宽。
    /// 主人那台蓝牙耳机的名字（下面这条）就能把后面的「刷新」和说明气泡一起推出窗外。
    /// 本机只有一台名字很短的麦克风，光靠真机跑发现不了这一条，所以钉在这里。
    #[test]
    fn a_long_microphone_name_keeps_the_row_inside_the_window() {
        let long = "耳机麦克风 (Jabra Evolve2 65 Hands-Free AG Audio)";
        let snap = probe_with(full_cfg(), 580.0, 1500.0, |_| {}, Tab::General, |app| {
            app.mics = vec![audio::MicDevice {
                id: "wasapi:long".into(),
                label: long.into(),
            }];
            app.edit.options.mic_device = "wasapi:long".into();
        });
        let mut bad = Vec::new();
        for t in &snap.texts {
            if t.y > snap.h - BOTTOM_RESERVED {
                continue; // 底部操作条里的文字另算
            }
            if t.x + t.w > snap.w - 4.0 {
                bad.push(format!("{:?}：x={:.1} w={:.1}", t.s, t.x, t.w));
            }
        }
        assert!(
            bad.is_empty(),
            "长设备名把这一行撑出窗口了（「刷新」按钮会跟着飞出窗外点不到）：\n{}",
            bad.join("\n")
        );
    }

    /// 任何一页、任何一段文字都不许超出窗口（回归：右对齐的说明文字被画到
    /// 窗外、被切掉一半）。
    ///
    /// egui 0.35 的右到左布局（`Layout::right_to_left`、连 `egui::Sides` 也一样）
    /// 在"横向行里再套一个布局"时会**把控件画到容器外**：实测一个 150px 宽的右对齐
    /// 说明文字被放在 x=467（窗口只有 500 宽），右半截直接看不见。所以界面上的右对齐
    /// 一律自己算宽度（见 `push_to_end`），这条用例就是那把尺子。
    /// 最小窗口（520，见 `main.rs` 的 min_inner_size）也要量一遍：说明文字的
    /// 宽度预算在窄窗口下最紧。
    #[test]
    fn no_text_overflows_the_window() {
        let mut bad = Vec::new();
        for (w, h) in [(580.0, 1500.0), (520.0, 560.0)] {
            for tab in Tab::ALL {
                let snap = probe_tab(Config::default(), w, h, |_| {}, tab);
                for t in &snap.texts {
                    if t.y > snap.h - BOTTOM_RESERVED {
                        continue; // 底部操作条里的文字另算（它本来就贴底）
                    }
                    if t.x + t.w > snap.w - 4.0 {
                        bad.push(format!(
                            "页签 {:?}（{w}×{h}）：x={:.1} w={:.1} right={:.1}：{:?}",
                            tab,
                            t.x,
                            t.w,
                            t.x + t.w,
                            t.s
                        ));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "有文字超出窗口：\n{}", bad.join("\n"));
    }

    /// 底部区域（反馈条 + 按钮行）里的文字也不许超出窗口
    /// （版本号以前就画出去了一截）
    #[test]
    fn no_bottom_bar_text_overflows() {
        let snap = probe(Config::default(), 580.0, 1500.0, |_| {});
        let mut bad = Vec::new();
        for t in &snap.texts {
            if t.y <= snap.h - BOTTOM_RESERVED {
                continue;
            }
            if t.x + t.w > snap.w - 4.0 {
                bad.push(format!(
                    "x={:.1} w={:.1} right={:.1}：{:?}",
                    t.x,
                    t.w,
                    t.x + t.w,
                    t.s
                ));
            }
        }
        assert!(bad.is_empty(), "底部区域有文字超出窗口：\n{}", bad.join("\n"));
    }

    /// 快捷键设得很长时，品牌栏和通用页都不能出问题：副标题**截断**（不换行、
    /// 不撑高品牌栏）、键帽**截断**（超长组合不会把「重新录制」挤出窗口），
    /// 所有文字都留在窗口内。
    #[test]
    fn a_long_hotkey_keeps_the_header_in_shape() {
        let cfg = Config {
            hotkey: "ctrl+alt+shift+win+pagedown".into(),
            ..Config::default()
        };
        // 通用页才有键帽行；语音服务页只出副标题，两页都量
        let snap = probe_tab(cfg, 580.0, 1500.0, |_| {}, Tab::General);
        let mut bad = Vec::new();
        for t in &snap.texts {
            if t.x + t.w > snap.w - 4.0 {
                bad.push(format!(
                    "x={:.1} w={:.1} right={:.1}：{:?}",
                    t.x,
                    t.w,
                    t.x + t.w,
                    t.s
                ));
            }
        }
        assert!(
            bad.is_empty(),
            "快捷键很长时有文字被挤出窗口：\n{}",
            bad.join("\n")
        );
    }

    /// 底栏多了「检查更新」之后：按钮要在、版本号不许被挤出去（最窄窗口也要量）。
    ///
    /// 底栏是固定高度的一行，塞第四个按钮最容易把最右边的版本号顶出窗口 ——
    /// 而"版本号看不见"直接毁掉主人分辨"我测的到底是不是新版"的手段（见 Cargo.toml
    /// 里那句注释）。所以两种窗口宽度都要量。
    #[test]
    fn bottom_bar_fits_the_update_button() {
        for w in [580.0, 520.0] {
            // a) 平时：4 个按钮
            check_bottom_bar(w, false);
            // b) 更新失败时多出一个「手动下载」（第 5 个按钮）—— 最容易把版本号挤出去
            check_bottom_bar(w, true);
        }
    }

    /// 量一次底栏：版本号必须在窗口内，且「检查更新」按钮在底部行里
    fn check_bottom_bar(w: f32, failed: bool) {
        let snap = probe_with(
            full_cfg(),
            w,
            1500.0,
            |_| {},
            Tab::Service,
            |app| {
                if failed {
                    app.update = UpdateState::Failed("下载失败：等待超时".into());
                }
            },
        );
        let y = snap
            .text_y("检查更新")
            .unwrap_or_else(|| panic!("{w} 宽时底栏没有「检查更新」按钮"));
        assert!(
            y >= snap.h - BOTTOM_RESERVED,
            "{w} 宽时「检查更新」不在底部按钮行里"
        );
        if failed {
            assert!(
                snap.text_y("手动下载").is_some(),
                "{w} 宽时更新失败却没有「手动下载」兜底按钮"
            );
        }
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let t = snap
            .texts
            .iter()
            .find(|t| t.s == version)
            .unwrap_or_else(|| panic!("{w} 宽（失败={failed}）时版本号 {version} 不见了"));
        assert!(
            t.x + t.w <= snap.w - 4.0,
            "{w} 宽（失败={failed}）时版本号被挤出窗口（x={:.1} w={:.1}）",
            t.x,
            t.w
        );
        let y = snap
            .text_y("检查更新")
            .unwrap_or_else(|| panic!("{w} 宽时底栏没有「检查更新」按钮"));
        assert!(
            y >= snap.h - BOTTOM_RESERVED,
            "{w} 宽时「检查更新」不在底部按钮行里"
        );
    }

    /// 有内容和提示时反馈条照常工作：结果显示在底部、不撑破窗口
    #[test]
    fn result_strip_shows_results_and_stays_in_shape() {
        let snap = probe(full_cfg(), 580.0, 1500.0, |shared| {
            shared.update(|s| {
                s.last_result = "今天天气不错，出去走走吧。".into();
                s.text = s.last_result.clone();
                s.kept_on_clipboard = true;
            });
        });
        // 结果本体要在底部区显示出来（不必完整，截断合法）
        let strip_y = snap
            .text_y("最近识别")
            .expect("底部必须有「最近识别」反馈条");
        assert!(
            strip_y >= snap.h - BOTTOM_RESERVED,
            "有结果时反馈条也必须钉在底部"
        );
        let mut bad = Vec::new();
        for t in &snap.texts {
            if t.y <= snap.h - BOTTOM_RESERVED {
                continue;
            }
            if t.x + t.w > snap.w - 4.0 {
                bad.push(format!("x={:.1} w={:.1} right={:.1}：{:?}", t.x, t.w, t.x + t.w, t.s));
            }
        }
        assert!(bad.is_empty(), "反馈条有文字超出窗口：\n{}", bad.join("\n"));
    }
}
