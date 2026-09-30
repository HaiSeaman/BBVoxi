//! 设置窗口。
//!
//! 设计基调取自应用图标本身（靛蓝→青绿的渐变），克制使用一个强调色：
//! 顶部品牌栏 + 卡片分区 + 底部固定操作条；所有颜色来自 `Palette`，
//! 跟随系统深浅色自动切换。

use crate::autostart;
use crate::config::{Config, Provider, TENCENT_ENGINES, TENCENT_URL};
use crate::hotkey::{self, Hotkey};
use crate::session::{Cmd, Shared, Snapshot};
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
        Self {
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
            pending_mods: None,
            painted_at: Instant::now(),
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
        self.handle_capture(ui.ctx());
        let p = Palette::of(ui.visuals().dark_mode);
        apply_widget_style(ui, &p);

        // 关窗确认画在最上层（有没保存的改动时才会出现）
        self.close_confirm(ui.ctx(), &p);

        // 先放底部固定操作条，中央区域再吃掉剩下的空间（egui 的面板顺序要求）
        egui::Panel::bottom("bbvoxi_actions")
            .frame(
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| self.actions_bar(ui, &p));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    // 滚动条平时不占位置（滚轮照样能滚）：界面更干净，
                    // 而且省下的一条竖向空间正好给内容用
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.add_space(2.0);
                        self.header(ui, &p);
                        ui.add_space(10.0);
                        // 反馈区紧跟品牌栏：测试录音时不用滚动就能看到实时文字。
                        // 旧版把它排在三张配置卡之后，768 高的笔记本上整块看不见 ——
                        // 恰恰是最需要它的机器上失效（报告第 1 条）。
                        self.result_card(ui, &p);
                        ui.add_space(8.0);
                        // 配置合成一张卡：说明文字收进悬停气泡，5 张卡变 2 张
                        self.settings_card(ui, &p);
                        ui.add_space(6.0);
                    });
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

        // 自己算宽度推到行尾（见 `push_to_end`：egui 的右对齐布局会画到容器外）
        let pill_w = text_width(ui, text, egui::TextStyle::Small) + 7.0 + 2.0 + 20.0 + 4.0;
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

    // —— 设置卡（服务商 / 凭据 / 快捷键 / 识别选项 / 通用 合成一张）——

    /// 一张卡装下所有设置：标签左、字段右，说明全部收进悬停气泡。
    ///
    /// 为什么合并：旧版把同样的内容摊成 3 张卡 + 大量说明小字，内容总高 1274px，
    /// 768 高的笔记本要滚 1.4 屏；而真正该看的「最近识别」被挤到屏幕外。
    /// 合并后按 900 高的窗口算，一屏就能看全（见 `layout` 模块的实测用例）。
    fn settings_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            labeled_row(ui, "服务商", p, |ui| {
                provider_selector(ui, p, &mut self.edit.provider);
            });

            // 凭据跟着服务商变。`push_id` 把三套字段的控件 id 分开：
            // 否则切服务商时 egui 会把上一套的焦点/光标状态带到下一套字段上。
            ui.push_id(self.edit.provider.label(), |ui| self.credentials(ui, p));

            sep(ui, p);
            self.hotkey_group(ui, p);
            sep(ui, p);

            // 识别选项（说明收进气泡）
            option_row(ui, p, TIP_LIVE_TYPING, |ui| {
                ui.checkbox(
                    &mut self.edit.options.live_typing,
                    body("边说话边打字（实时输入）"),
                );
            });
            option_row(ui, p, TIP_CLIPBOARD_FALLBACK, |ui| {
                ui.checkbox(
                    &mut self.edit.options.clipboard_fallback,
                    body("输入被拒时用剪贴板粘贴兜底"),
                );
            });
            option_row(ui, p, TIP_KEEP_ON_CLIPBOARD, |ui| {
                ui.checkbox(
                    &mut self.edit.options.keep_on_clipboard,
                    body("识别结果总留一份到剪贴板（推荐）"),
                );
            });

            // 标点 / 顺滑只有部分服务商支持：不支持时禁用并说明 ——
            // 不能给主人一个看起来能拨、实际不接线的开关
            let (punc_ok, smooth_ok) = match self.edit.provider {
                Provider::Qwen => (true, false),
                Provider::Doubao => (true, true),
                Provider::Tencent => (false, true),
            };
            option_row(
                ui,
                p,
                if punc_ok { TIP_PUNCTUATION } else { TIP_PUNCTUATION_OFF },
                |ui| {
                    ui.add_enabled(
                        punc_ok,
                        egui::Checkbox::new(
                            &mut self.edit.options.auto_punctuation,
                            body("自动添加标点"),
                        ),
                    );
                },
            );
            option_row(
                ui,
                p,
                if smooth_ok { TIP_SMOOTH } else { TIP_SMOOTH_OFF },
                |ui| {
                    ui.add_enabled(
                        smooth_ok,
                        egui::Checkbox::new(
                            &mut self.edit.options.smooth,
                            body("口语顺滑（去除重复与语气词）"),
                        ),
                    );
                },
            );

            sep(ui, p);
            option_row(ui, p, TIP_AUTOSTART, |ui| self.autostart_checkbox(ui));
        });
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
                option_row(ui, p, TIP_HIGH_ACCURACY, |ui| {
                    ui.checkbox(
                        &mut self.edit.doubao.high_accuracy,
                        body("高精度模式（整句二次识别，延迟略高）"),
                    );
                });
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

    /// 快捷键：键帽 + 「重新录制」+ 悬停说明（那两段长解释不再占页面高度）
    fn hotkey_group(&mut self, ui: &mut egui::Ui, p: &Palette) {
        labeled_row(ui, "快捷键", p, |ui| {
            if self.capturing {
                ui.label(body("请按下新的组合键（松开后生效）").color(p.warn));
                if ghost_button(ui, p, "取消").clicked() {
                    self.end_capture();
                }
                small_at_end(ui, p, "Esc 也可取消");
                return;
            }
            let display = hotkey::parse(&self.edit.hotkey)
                .map(|h| h.display())
                .unwrap_or_else(|_| self.edit.hotkey.clone());
            egui::Frame::new()
                .fill(p.field)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 4))
                .show(ui, |ui| {
                    // 等宽字体当键帽，反引号之类的符号更好认
                    ui.label(RichText::new(display).monospace().strong().color(p.text));
                });
            if ghost_button(ui, p, "重新录制").clicked() {
                self.begin_capture(ui.ctx());
            }
            push_to_end(ui, INFO_W);
            info(ui, p, TIP_HOTKEY);
        });
        if let Some(err) = self.hotkey_error.clone() {
            ui.label(small(err).color(p.danger));
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
                self.confirm_close = false;
                self.hide_requested = true;
            }
            Some(Choice::Cancel) => self.confirm_close = false,
            // 对话框开着、主人还没选
            None => {}
        }
    }

    /// 开机自启：勾选立即写注册表（失败会回滚勾选）
    fn autostart_checkbox(&mut self, ui: &mut egui::Ui) {
        let before = self.autostart;
        ui.checkbox(&mut self.autostart, body("开机自启"));
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

    /// 最近识别：标题一行 + 正文一行（测试模式的承诺留在明面上，其余进气泡）
    fn result_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        let hotkey_text = hotkey::parse(&self.edit.hotkey)
            .map(|h| h.display())
            .unwrap_or_else(|_| self.edit.hotkey.clone());
        card(p, ui, |ui, p| {
            ui.horizontal(|ui| {
                ui.label(body("最近识别").strong().color(p.text));
                // 这句必须留在明面上（不能只塞进气泡）：测试结果不会外打是主人
                // 最需要确认的一件事，藏起来他反而不敢按
                small_at_end(ui, p, "只显示在这里，不会打字出去");
            });
            ui.add_space(6.0);
            if snap.recording {
                // 录音中的呼吸点：整窗唯一的动效，一眼看出"正在听"
                let t = ui.ctx().input(|i| i.time);
                let pulse = (0.55 + 0.45 * (t * 4.0).sin()) as f32;
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        rect.center(),
                        4.0,
                        p.danger.gamma_multiply(0.35 + 0.65 * pulse),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        body(format!("录音中 {}", clock(snap.elapsed())))
                            .strong()
                            .color(p.danger),
                    );
                });
                let shown = if !snap.text.is_empty() {
                    snap.text.clone()
                } else if !snap.hint.is_empty() {
                    snap.hint.clone()
                } else {
                    "请说话…".to_string()
                };
                ui.label(body(shown).color(p.text));
            } else if let Some(err) = &snap.error {
                ui.label(body(err).color(p.danger));
            } else if let Some(notice) = &snap.notice {
                // 结果已放进剪贴板（通常是录音途中切了窗口）。用提醒色而不是危险色：
                // 这不是故障，但主人得知道"这次的字没自动打出去，去 Ctrl+V 粘"。
                ui.label(body(notice).color(p.warn));
                if !snap.last_result.is_empty() {
                    ui.add_space(4.0);
                    ui.label(body(&snap.last_result).color(p.text));
                }
            } else if !snap.last_result.is_empty() {
                ui.label(body(&snap.last_result).color(p.text));
            } else {
                ui.label(
                    body(format!(
                        "还没有识别记录。按住 {hotkey_text} 说句话，或点下面「测试识别」。"
                    ))
                    .color(p.muted),
                );
            }
            // 「结果在剪贴板」这句话只说给"找不到时想找"的主人听，而且**只在真的
            // 留住了**（写完又回读核对过）的时候才说 —— 以前这里只看"开关开着 +
            // 有结果"，于是复制失败的那一次也照样写着"留在了剪贴板"，主人照着去
            // Ctrl+V 却什么也粘不出来（正是他报的那个坑，只是从弹窗搬进了窗口）。
            if snap.kept_on_clipboard && !snap.recording && !snap.last_result.is_empty() {
                ui.add_space(3.0);
                ui.label(small("这份结果也留在了剪贴板，随时可以 Ctrl+V").color(p.muted));
            }
        });
    }

    // —— 底部操作条 ——

    fn actions_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // 状态条独占一行：和按钮挤在水平布局里会被长错误消息挤压甚至截断。
        // 而且 10 秒后它自己就退场了（见 `Status`），不会永远占着这一行。
        if let Some(status) = self.status.clone() {
            let color = if status.ok { p.ok } else { p.danger };
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.0, color);
                ui.add_space(2.0);
                ui.add(egui::Label::new(body(status.text).color(color).strong()).truncate());
            });
            ui.add_space(6.0);
        }
        let recording = self.shared.snapshot().recording;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !recording,
                    egui::Button::new(button_text("测试识别（5 秒）").color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(130.0, 34.0)),
                )
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
            if ui
                .add(
                    egui::Button::new(button_text("保存").strong().color(p.on_accent))
                        .fill(p.accent)
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(104.0, 34.0)),
                )
                .clicked()
            {
                self.save();
            }
            if ui
                .add(
                    egui::Button::new(button_text("项目地址").color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(92.0, 34.0)),
                )
                .clicked()
            {
                if let Err(e) = open_project_page() {
                    self.set_status(false, format!("打开项目地址失败：{e}"));
                }
            }
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

const TIP_HIGH_ACCURACY: &str = "开启后先用实时模型听着、松手再做一次整句识别：\
更准一点，但会晚一两秒出结果。";

const TIP_APP_ID: &str = "腾讯云语音识别的 App ID（控制台里的数字 ID）。";

const TIP_TENCENT_ENGINE: &str = "腾讯云的识别引擎。Hy-ASR-3.0-preview 只支持 60 秒以内的语音\
（建议说到 55 秒就停）；16k_zh_en 支持中英混说。";

const TIP_HOTKEY: &str = "修饰键 Ctrl / Alt / Shift / Win 随意搭配（1~4 个都行），主键支持字母、\
数字、F1~F24、空格、方向键、翻页键和符号键；也可以整组只用修饰键（如 Ctrl + Win，先按住 \
Ctrl 再按 Win，全部松开后生效）。按下时会拦截该组合键。\n\n\
只有单独一个普通键（如 a）和单独一个修饰键（如 Ctrl）不能当快捷键：\
前者会在所有程序里吞掉这个键，后者的「按住说话」和 Ctrl+C 分不开。";

const TIP_LIVE_TYPING: &str = "识别被修正时自动回退重打；关闭则只在松手后一次性输入。";

const TIP_CLIPBOARD_FALLBACK: &str = "系统真的拦下模拟按键时（如管理员权限窗口），\
改走 Ctrl+V 再试一次。";

const TIP_KEEP_ON_CLIPBOARD: &str = "字到底有没有打进目标程序无法核实，所以每次识别都留一份，\
随时可 Ctrl+V；代价是你原来复制的内容会被顶掉。";

const TIP_PUNCTUATION: &str = "识别结果自动补全逗号、句号等标点。";

const TIP_PUNCTUATION_OFF: &str = "腾讯云引擎由服务端决定，暂不支持此选项。";

const TIP_SMOOTH: &str = "过滤「嗯、啊」等语气词与重复表述。";

const TIP_SMOOTH_OFF: &str = "千问暂不支持此选项。";

const TIP_AUTOSTART: &str = "登录 Windows 后自动在后台待命，不弹窗口。";

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
fn push_to_end(ui: &mut egui::Ui, width: f32) {
    let pad = (ui.available_width() - width).max(0.0);
    ui.add_space(pad);
}

/// 行尾的小字（自己算位置，见 `push_to_end`）
fn small_at_end(ui: &mut egui::Ui, p: &Palette, text: &str) {
    let w = text_width(ui, text, egui::TextStyle::Small);
    push_to_end(ui, w);
    ui.label(small(text).color(p.muted));
}

/// 一个开关行：开关靠左，右侧留一个说明气泡
fn option_row(ui: &mut egui::Ui, p: &Palette, tip: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        add(ui);
        push_to_end(ui, INFO_W);
        info(ui, p, tip);
    });
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
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let url: Vec<u16> = PROJECT_URL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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
    Err("仅支持 Windows".into())
}

/// 服务商分段选择器：三家一眼看全，比下拉少一次点击
fn provider_selector(ui: &mut egui::Ui, p: &Palette, current: &mut Provider) {
    ui.horizontal(|ui| {
        let count = Provider::ALL.len() as f32;
        let width = (ui.available_width() - (count - 1.0) * 6.0) / count;
        for (i, provider) in Provider::ALL.into_iter().enumerate() {
            if i > 0 {
                ui.add_space(6.0);
            }
            let selected = *current == provider;
            let (fill, text_color, stroke) = if selected {
                (
                    // 用 `tint_over` 而不是就地写一个透明度：那个常量是被
                    // `palette_contrast_meets_wcag` 逐个校验过的；就地写死一个
                    // 数字，会让"真校验"漏掉真正画出来的那一对（评审抓到过一次）。
                    tint_over(p.card, p.accent),
                    p.accent,
                    Stroke::new(1.5, p.accent),
                )
            } else {
                (p.field, p.muted, Stroke::new(1.0, p.border))
            };
            let label = button_text(short_label(provider)).strong().color(text_color);
            if ui
                .add(
                    egui::Button::new(label)
                        .fill(fill)
                        .stroke(stroke)
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(width, 32.0)),
                )
                .clicked()
            {
                *current = provider;
            }
        }
    });
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
        let mut snap = Snapshot::default();
        snap.kept_on_clipboard = true; // 常态：结果照例留了一份
        snap.last_result = "你好".into();
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
        pub color: [u8; 4],
    }

    /// 一帧里画出来的一个矩形
    #[derive(Debug, Clone)]
    pub struct RectI {
        pub x: f32,
        pub y: f32,
        pub w: f32,
        pub h: f32,
        pub fill: [u8; 4],
        pub stroke: [u8; 4],
        pub sw: f32,
        pub cr: f32,
    }

    pub struct Snapshot {
        pub w: f32,
        pub h: f32,
        pub texts: Vec<Text>,
        pub rects: Vec<RectI>,
    }

    /// 面板底色是整屏宽的大矩形，不是"内容"，量内容时要把它排除
    const PANEL_MIN_W: f32 = 490.0;
    /// 底部操作条占窗口最底下这一块，量"内容总高"时整块排除
    const BOTTOM_RESERVED: f32 = 90.0;

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
                x: r.rect.min.x,
                y: r.rect.min.y,
                w: r.rect.width(),
                h: r.rect.height(),
                fill: r.fill.to_array(),
                stroke: r.stroke.color.to_array(),
                sw: r.stroke.width,
                cr: r.corner_radius.nw as f32,
            }),
            egui::Shape::Text(t) => {
                let size = t.galley.size();
                snap.texts.push(Text {
                    s: t.galley.text().to_owned(),
                    x: t.pos.x,
                    y: t.pos.y,
                    w: size.x,
                    h: size.y,
                    color: t.fallback_color.to_array(),
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
        require_cjk_font();
        let ctx = egui::Context::default();
        crate::setup_cjk_font(&ctx);
        setup_style(&ctx);
        ctx.set_theme(egui::ThemePreference::Light);
        let mut app = super::test_util::app(cfg, prepare);
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

    /// 方案 C 的验收线 1：**「最近识别」必须在首屏**。
    ///
    /// 窗口高 = 屏幕高 − 160（上限 900，见 `main.rs::window_geometry`）：
    /// 768 高的笔记本上可滚动区只有约 530px。旧版这一区起点在 599px ——
    /// 恰好是"最需要看实时文字"的机器上完全看不到（报告第 1 条问题）。
    #[test]
    fn result_card_is_on_the_first_screen() {
        let snap = probe(Config::default(), 500.0, 1500.0, |_| {});
        let top = snap
            .text_y("最近识别")
            .expect("界面上必须有「最近识别」这一区");
        assert!(
            top < 530.0,
            "「最近识别」起点 {top}px，768 高的笔记本（可见区约 530px）看不到它"
        );
        assert!(
            top < 220.0,
            "「最近识别」起点 {top}px —— 还能更靠前：它就该在品牌栏正下方"
        );
    }

    /// 方案 C 的验收线 2：内容总高压到 700px 以内、卡片从 5 张减到 2 张。
    #[test]
    fn content_is_compact() {
        let snap = probe(Config::default(), 500.0, 1500.0, |_| {});
        let h = snap.content_height();
        assert!(
            h <= 700.0,
            "内容总高 {h}px，没压到 700px 以内（报告实测旧版 1274px）"
        );
        assert_eq!(
            snap.card_count([255, 255, 255, 255]),
            2,
            "卡片数应为 2 张（最近识别 + 设置），旧的 5 张卡方案太占地方"
        );
    }

    /// 任何一段文字都不许超出窗口（回归：右对齐的说明文字被画到窗外、被切掉一半）。
    ///
    /// egui 0.35 的右到左布局（`Layout::right_to_left`、连 `egui::Sides` 也一样）
    /// 在"横向行里再套一个布局"时会**把控件画到容器外**：实测一个 150px 宽的右对齐
    /// 说明文字被放在 x=467（窗口只有 500 宽），右半截直接看不见。所以界面上的右对齐
    /// 一律自己算宽度（见 `push_to_end`），这条用例就是那把尺子。
    #[test]
    fn no_text_overflows_the_window() {
        let snap = probe(Config::default(), 500.0, 1500.0, |_| {});
        let mut bad = Vec::new();
        for t in &snap.texts {
            if t.y > snap.h - BOTTOM_RESERVED {
                continue; // 底部操作条里的文字另算（它本来就贴底）
            }
            if t.x + t.w > snap.w - 4.0 {
                bad.push(format!(
                    "x={:.1} w={:.1} right={:.1}（窗口宽 {:.0}）：{:?}",
                    t.x,
                    t.w,
                    t.x + t.w,
                    snap.w,
                    t.s
                ));
            }
        }
        assert!(bad.is_empty(), "有文字超出窗口：\n{}", bad.join("\n"));
    }

    /// 底部操作条里的文字也不许超出窗口（版本号以前就画出去了一截）
    #[test]
    fn no_bottom_bar_text_overflows() {
        let snap = probe(Config::default(), 500.0, 1500.0, |_| {});
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
        assert!(bad.is_empty(), "底部操作条有文字超出窗口：\n{}", bad.join("\n"));
    }

    /// 快捷键设得很长时，品牌栏也不能出问题：副标题**截断**（不换行、不撑高品牌栏），
    /// 所有文字都留在窗口内。
    ///
    /// 说明：评审怀疑"四个人修饰键 + 主键的长快捷键会把状态胶囊挤出窗口"。
    /// 女仆实测**不会**（副标题的截断宽度由布局自己兜住，胶囊稳在 478/500），
    /// 所以没有为它加"预留宽度"那种没必要的代码；这条用例留着，是为了兜住
    /// 以后再动品牌栏时的回归（比如把 truncate 去掉、让副标题换行把品牌栏撑高）。
    #[test]
    fn a_long_hotkey_keeps_the_header_in_shape() {
        let mut cfg = Config::default();
        cfg.hotkey = "ctrl+alt+shift+win+pagedown".into();
        let snap = probe(cfg, 500.0, 1500.0, |_| {});
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

    /// 有内容和提示时也不能撑破（结果卡会多一行字、状态胶囊会换成提醒色）
    #[test]
    fn content_stays_compact_with_a_result_shown() {
        let snap = probe(full_cfg(), 500.0, 1500.0, |shared| {
            shared.update(|s| {
                s.last_result = "今天天气不错，出去走走吧。".into();
                s.text = s.last_result.clone();
                s.kept_on_clipboard = true;
            });
        });
        let h = snap.content_height();
        assert!(h <= 760.0, "带结果和提示时内容总高 {h}px，超了");
        assert!(
            snap.text_y("最近识别").is_some_and(|y| y < 530.0),
            "有结果时「最近识别」也必须留在首屏"
        );
    }

    /// 导出这一帧的绘制指令（JSON），给 `docs/_draw_layout.py` 画验收图用。
    ///
    /// 标 `#[ignore]`：它只产出验收素材，不进常规测试集。
    /// 手动跑：`cargo test --release dump_layout_for_review -- --ignored --nocapture`
    #[test]
    #[ignore = "产出验收图用的 JSON，手动跑：cargo test dump_layout_for_review -- --ignored"]
    fn dump_layout_for_review() {
        let snap = probe(Config::default(), 500.0, 1500.0, |_| {});
        let doc = serde_json::json!({
            "win_w": snap.w,
            "win_h": snap.h,
            "rects": snap.rects.iter().map(|r| serde_json::json!({
                "x": r.x, "y": r.y, "w": r.w, "h": r.h,
                "fill": r.fill, "stroke": r.stroke, "sw": r.sw, "cr": r.cr,
            })).collect::<Vec<_>>(),
            "texts": snap.texts.iter().map(|t| serde_json::json!({
                "s": t.s, "x": t.x, "y": t.y, "w": t.w, "h": t.h, "color": t.color,
            })).collect::<Vec<_>>(),
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("layout_now.json");
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        println!("已导出：{}", path.display());
    }
}
