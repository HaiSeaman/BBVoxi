//! 设置窗口。
//!
//! 设计基调取自应用图标本身（靛蓝→青绿的渐变），克制使用一个强调色：
//! 顶部品牌栏 + 卡片分区 + 底部固定操作条；所有颜色来自 `Palette`，
//! 跟随系统深浅色自动切换。

use crate::autostart;
use crate::config::{Config, Provider, TENCENT_ENGINES, TENCENT_URL};
use crate::hotkey::{self, Hotkey};
use crate::session::{Cmd, Shared};
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
                ok: Color32::from_rgb(0x12, 0xA1, 0x50),
                warn: Color32::from_rgb(0xC7, 0x7A, 0x00),
                danger: Color32::from_rgb(0xD9, 0x2D, 0x20),
            }
        }
    }
}

/// 全局控件风格（圆角、内边距、交互高度）
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

    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.weak_bg_fill = p.field;
    v.widgets.inactive.bg_fill = p.field;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.muted);
    v.widgets.hovered.weak_bg_fill = p.field;
    v.widgets.hovered.bg_fill = p.field;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.55));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.weak_bg_fill = p.field;
    v.widgets.active.bg_fill = p.field;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
}

pub struct SettingsApp {
    edit: Config,
    shared_cfg: Arc<Mutex<Config>>,
    shared: Arc<Shared>,
    cmd_tx: UnboundedSender<Cmd>,
    paused: Arc<AtomicBool>,
    show_keys: bool,
    status: Option<(bool, String)>,
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
        Self {
            edit: cfg,
            shared_cfg,
            shared,
            cmd_tx,
            paused,
            show_keys: false,
            status: None,
            autostart,
            applied_autostart: autostart,
            capturing: false,
            hotkey_error: None,
            pending_mods: None,
            painted_at: Instant::now(),
        }
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
        self.handle_capture(ui.ctx());
        let p = Palette::of(ui.visuals().dark_mode);
        apply_widget_style(ui, &p);

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
                    .show(ui, |ui| {
                        ui.add_space(2.0);
                        self.header(ui, &p);
                        ui.add_space(10.0);
                        self.service_card(ui, &p);
                        ui.add_space(8.0);
                        self.hotkey_card(ui, &p);
                        ui.add_space(8.0);
                        // 反馈区放在配置区之前：测试录音时不用滚动就能看到实时文字
                        self.result_card(ui, &p);
                        ui.add_space(8.0);
                        self.options_card(ui, &p);
                        ui.add_space(8.0);
                        // 通用沉底：设一次就不动的开关，不挤占首屏
                        self.general_card(ui, &p);
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
                egui::vec2(44.0, 44.0),
            )));
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("BBVoxi").size(21.0).strong().color(p.text));
                ui.label(
                    RichText::new(format!("按住 {hotkey_text} 说话，松开自动输入"))
                        .size(12.0)
                        .color(p.muted),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.status_pill(ui, p);
            });
        });
    }

    fn status_pill(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        let (color, text) = if snap.recording {
            (p.danger, "录音中".to_string())
        } else if snap.error.is_some() {
            (p.danger, "出错了".to_string())
        } else if self.capturing {
            // 与「录音中」一字之差完全分不清，改成明确指向快捷键
            (p.warn, "改键中".to_string())
        } else if !self.edit.credentials_ready() {
            (p.warn, "未配置".to_string())
        } else {
            (p.ok, "就绪".to_string())
        };

        egui::Frame::new()
            .fill(color.gamma_multiply(0.14))
            .corner_radius(CornerRadius::same(11))
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, color);
                    ui.add_space(2.0);
                    ui.label(RichText::new(text).size(12.0).color(color).strong());
                });
            });
    }

    // —— 卡片 ——

    fn service_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            section_title(ui, p, "语音服务");
            ui.add_space(10.0);

            ui.label(RichText::new("服务商").size(12.0).color(p.muted));
            provider_selector(ui, p, &mut self.edit.provider);
            ui.add_space(8.0);

            ui.push_id(self.edit.provider.label(), |ui| match self.edit.provider {
                Provider::Qwen => {
                    self.secret_field(ui, p, "API Key", "qwen_key");
                    self.text_field(ui, p, "模型名称", "qwen_model");
                    ui.collapsing("高级（业务空间专属域名）", |ui| {
                        self.endpoint_field(ui, p);
                        endpoint_hint(ui, p, &self.edit.qwen.base_url.clone());
                    });
                }
                Provider::Doubao => {
                    self.secret_field(ui, p, "API Key", "doubao_key");
                    self.text_field(ui, p, "模型 / 资源 ID", "doubao_res");
                    ui.checkbox(
                        &mut self.edit.doubao.high_accuracy,
                        RichText::new("高精度模式（整句二次识别，延迟略高）").size(13.0),
                    );
                    endpoint_hint(ui, p, self.edit.doubao.endpoint());
                }
                Provider::Tencent => {
                    self.text_field(ui, p, "App ID", "tc_app");
                    self.secret_field(ui, p, "Secret ID", "tc_sid");
                    self.secret_field(ui, p, "Secret Key", "tc_skey");
                    ui.label(RichText::new("引擎模型").size(12.0).color(p.muted));
                    egui::ComboBox::from_id_salt("tencent_engine")
                        .selected_text(self.edit.tencent.engine_model_type.clone())
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for engine in TENCENT_ENGINES {
                                ui.selectable_value(
                                    &mut self.edit.tencent.engine_model_type,
                                    engine.to_string(),
                                    engine,
                                );
                            }
                        });
                    ui.add_space(6.0);
                    endpoint_hint(
                        ui,
                        p,
                        &format!("{TENCENT_URL}/<App ID>?签名参数（自动计算）"),
                    );
                    ui.label(
                        RichText::new("Hy-ASR-3.0-preview 仅支持 60 秒内语音，上限建议 55 秒")
                            .size(11.0)
                            .color(p.muted),
                    );
                }
            });
        });
    }

    fn hotkey_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            section_title(ui, p, "快捷键");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if self.capturing {
                    ui.label(
                        RichText::new("请按下新的组合键（松开后生效）")
                            .size(14.0)
                            .color(p.warn),
                    );
                    if ghost_button(ui, p, "取消").clicked() {
                        self.end_capture();
                    }
                    ui.label(RichText::new("Esc 也可取消").size(11.5).color(p.muted));
                } else {
                    let display = hotkey::parse(&self.edit.hotkey)
                        .map(|h| h.display())
                        .unwrap_or_else(|_| self.edit.hotkey.clone());
                    egui::Frame::new()
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(14, 7))
                        .show(ui, |ui| {
                            // 等宽字体当键帽，反引号之类的符号更好认
                            ui.label(
                                RichText::new(display)
                                    .size(15.0)
                                    .strong()
                                    .monospace()
                                    .color(p.text),
                            );
                        });
                    if ghost_button(ui, p, "重新录制").clicked() {
                        self.begin_capture(ui.ctx());
                    }
                }
            });
            if let Some(err) = self.hotkey_error.clone() {
                ui.add_space(4.0);
                ui.label(RichText::new(err).size(12.0).color(p.danger));
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "修饰键 Ctrl / Alt / Shift / Win 随意搭配（1~4 个都行），主键支持字母、数字、\
                     F1~F24、空格、方向键、翻页键和符号键；也可以整组只用修饰键（如 Ctrl + Win，\
                     先按住 Ctrl 再按 Win，全部松开后生效）。按下时会拦截该组合键。",
                )
                .size(11.5)
                .color(p.muted),
            );
            ui.label(
                RichText::new(
                    "只有单独一个普通键（如 a）和单独一个修饰键（如 Ctrl）不能当快捷键：\
                     前者会在所有程序里吞掉这个键，后者的「按住说话」和 Ctrl+C 分不开。",
                )
                .size(11.5)
                .color(p.muted),
            );
        });
    }

    fn options_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            section_title(ui, p, "识别选项");
            ui.add_space(8.0);
            // 实时输入是客户端行为，三家服务商都可用
            ui.checkbox(
                &mut self.edit.options.live_typing,
                RichText::new("边说话边打字（实时输入）").size(13.0),
            );
            ui.label(
                RichText::new("识别被修正时自动回退重打；关闭则只在松手后一次性输入")
                    .size(11.0)
                    .color(p.muted),
            );
            ui.add_space(4.0);

            // 剪贴板相关的两个开关都在客户端起作用，与选哪家服务商无关
            ui.checkbox(
                &mut self.edit.options.clipboard_fallback,
                RichText::new("输入被拒时用剪贴板粘贴兜底").size(13.0),
            );
            ui.label(
                RichText::new(
                    "目标程序不认模拟按键时改走 Ctrl+V；粘贴成功后会还原你原来的剪贴板内容",
                )
                .size(11.0)
                .color(p.muted),
            );
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.edit.options.keep_on_clipboard,
                RichText::new("识别结果总留一份到剪贴板").size(13.0),
            );
            ui.label(
                RichText::new(
                    "输入成功也留一份，随时可以 Ctrl+V；开启后不再还原你原来的剪贴板内容",
                )
                .size(11.0)
                .color(p.muted),
            );
            ui.add_space(4.0);

            // 标点 / 顺滑只有部分服务商支持：不支持时禁用并说明，
            // 不能给用户一个看起来能拨、实际不接线的开关
            let (punc_ok, smooth_ok) = match self.edit.provider {
                Provider::Qwen => (true, false),
                Provider::Doubao => (true, true),
                Provider::Tencent => (false, true),
            };
            ui.add_enabled(
                punc_ok,
                egui::Checkbox::new(
                    &mut self.edit.options.auto_punctuation,
                    RichText::new("自动添加标点").size(13.0),
                ),
            );
            ui.label(
                RichText::new(if punc_ok {
                    "识别结果自动补全逗号、句号等标点"
                } else {
                    "腾讯云引擎由服务端决定，暂不支持此选项"
                })
                .size(11.0)
                .color(p.muted),
            );
            ui.add_space(4.0);
            ui.add_enabled(
                smooth_ok,
                egui::Checkbox::new(
                    &mut self.edit.options.smooth,
                    RichText::new("口语顺滑（去除重复与语气词）").size(13.0),
                ),
            );
            ui.label(
                RichText::new(if smooth_ok {
                    "过滤「嗯、啊」等语气词与重复表述"
                } else {
                    "千问暂不支持此选项"
                })
                .size(11.0)
                .color(p.muted),
            );
        });
    }

    /// 通用：与识别无关的开关（沉底放）。开机自启之前挤在底部操作条里，
    /// 和瞬态状态消息、动作按钮混在一起，职责不清，挪进卡片。
    fn general_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            section_title(ui, p, "通用");
            ui.add_space(8.0);
            self.autostart_checkbox(ui);
            ui.label(
                RichText::new("登录 Windows 后自动在后台待命，不弹窗口")
                    .size(11.0)
                    .color(p.muted),
            );
        });
    }

    /// 开机自启：勾选立即写注册表（失败会回滚勾选）
    fn autostart_checkbox(&mut self, ui: &mut egui::Ui) {
        let before = self.autostart;
        ui.checkbox(&mut self.autostart, RichText::new("开机自启").size(13.0));
        if self.autostart != before && self.autostart != self.applied_autostart {
            match autostart::set(self.autostart) {
                Ok(()) => {
                    self.applied_autostart = self.autostart;
                    self.status = Some((
                        true,
                        if self.autostart {
                            "已开启开机自启".into()
                        } else {
                            "已关闭开机自启".into()
                        },
                    ));
                }
                Err(e) => {
                    self.autostart = self.applied_autostart;
                    self.status = Some((false, format!("{e:#}")));
                }
            }
        }
    }

    fn result_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        card(p, ui, |ui, p| {
            section_title(ui, p, "最近识别");
            ui.add_space(8.0);
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
                        RichText::new(format!("录音中 {}", clock(snap.elapsed())))
                            .size(13.0)
                            .strong()
                            .color(p.danger),
                    );
                });
                let body = if !snap.text.is_empty() {
                    snap.text.clone()
                } else if !snap.hint.is_empty() {
                    snap.hint.clone()
                } else {
                    "请说话…".to_string()
                };
                ui.label(RichText::new(body).size(14.0).color(p.text));
            } else if let Some(err) = &snap.error {
                ui.label(RichText::new(err).size(13.0).color(p.danger));
            } else if !snap.last_result.is_empty() {
                ui.label(RichText::new(&snap.last_result).size(14.0).color(p.text));
            } else {
                ui.label(
                    RichText::new("还没有识别记录。点下面「测试识别」说一句话试试。")
                        .size(13.0)
                        .color(p.muted),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new("测试结果只显示在这里，不会往其他程序里打字。")
                        .size(11.0)
                        .color(p.muted),
                );
            }
        });
    }

    // —— 底部操作条 ——

    fn actions_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // 状态条独占一行：和按钮挤在水平布局里会被长错误消息挤压甚至截断
        if let Some((ok, msg)) = self.status.clone() {
            let color = if ok { p.ok } else { p.danger };
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.0, color);
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(RichText::new(msg).size(12.0).color(color).strong())
                        .truncate(),
                );
            });
            ui.add_space(6.0);
        }
        let recording = self.shared.snapshot().recording;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !recording,
                    egui::Button::new(RichText::new("测试识别（5 秒）").size(13.0).color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(140.0, 34.0)),
                )
                .clicked()
            {
                if !self.edit.credentials_ready() {
                    self.status = Some((false, "请先填写当前服务商的凭据".into()));
                } else {
                    self.sync_to_shared();
                    let _ = self.cmd_tx.send(Cmd::Test);
                    self.status = Some((true, "已开始 5 秒测试".into()));
                }
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("保存").size(14.0).strong().color(p.on_accent))
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
                    egui::Button::new(RichText::new("项目地址").size(13.0).color(p.text))
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .min_size(egui::vec2(96.0, 34.0)),
                )
                .clicked()
            {
                if let Err(e) = open_project_page() {
                    self.status = Some((false, format!("打开项目地址失败：{e}")));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(p.muted),
                );
            });
        });
    }

    // —— 行为 ——

    fn save(&mut self) {
        // 快捷键要立刻生效，不能等重启（否则用户改完按了没反应，以为坏了）
        match hotkey::apply_from_config(&self.edit.hotkey) {
            Ok(hk) => {
                self.edit.hotkey = hk.to_config();
                self.hotkey_error = None;
            }
            Err(e) => {
                self.hotkey_error = Some(format!("快捷键无效：{e}"));
                self.status = Some((false, "快捷键格式不对，未保存".into()));
                return;
            }
        }
        match self.edit.save() {
            Ok(()) => {
                self.sync_to_shared();
                self.status = Some((true, "已保存".into()));
            }
            Err(e) => self.status = Some((false, format!("保存失败：{e:#}"))),
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
                self.status = Some((true, format!("快捷键已改为 {}", hk.display())));
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

    fn text_field(&mut self, ui: &mut egui::Ui, p: &Palette, label: &str, id: &str) {
        ui.label(RichText::new(label).size(12.0).color(p.muted));
        let value = match id {
            "qwen_model" => &mut self.edit.qwen.model,
            "doubao_res" => &mut self.edit.doubao.resource_id,
            "tc_app" => &mut self.edit.tencent.app_id,
            _ => return,
        };
        field_edit(ui, value, false);
        ui.add_space(6.0);
    }

    fn secret_field(&mut self, ui: &mut egui::Ui, p: &Palette, label: &str, id: &str) {
        // 标签行右侧放"显示/隐藏"小按钮：入口紧贴字段本身，
        // 替代原先挂在卡片底部的全局「显示密钥明文」复选框
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(12.0).color(p.muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let eye = if self.show_keys { "隐藏" } else { "显示" };
                if small_button(ui, p, eye).clicked() {
                    self.show_keys = !self.show_keys;
                }
            });
        });
        let value = match id {
            "qwen_key" => &mut self.edit.qwen.api_key,
            "doubao_key" => &mut self.edit.doubao.api_key,
            "tc_sid" => &mut self.edit.tencent.secret_id,
            "tc_skey" => &mut self.edit.tencent.secret_key,
            _ => return,
        };
        field_edit(ui, value, !self.show_keys);
        ui.add_space(6.0);
    }

    fn endpoint_field(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.label(RichText::new("接口地址").size(12.0).color(p.muted));
        field_edit(ui, &mut self.edit.qwen.base_url, false);
        ui.add_space(6.0);
    }
}

// —— 无状态小部件 ——

fn card<R>(p: &Palette, ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui, &Palette) -> R) -> R {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui, p)
        })
        .inner
}

/// 分区标题：左侧一根强调色短竖条（替代装饰性编号）
fn section_title(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(2), p.accent);
        ui.add_space(4.0);
        ui.label(RichText::new(text).size(13.5).strong().color(p.text));
    });
}

fn field_edit(ui: &mut egui::Ui, value: &mut String, password: bool) {
    let width = ui.available_width();
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(width)
            .password(password)
            .margin(Margin::symmetric(10, 7)),
    );
}

fn endpoint_hint(ui: &mut egui::Ui, p: &Palette, url: &str) {
    ui.label(
        RichText::new(format!("内置接口 {url}"))
            .size(11.5)
            .color(p.muted)
            .monospace(),
    );
}

fn ghost_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(13.0).color(p.text))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// 贴在字段标签行里的小按钮（如"显示/隐藏"密钥）
fn small_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(11.0).color(p.muted))
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
                    p.accent.gamma_multiply(0.14),
                    p.accent,
                    Stroke::new(1.5, p.accent),
                )
            } else {
                (p.field, p.muted, Stroke::new(1.0, p.border))
            };
            let label = RichText::new(short_label(provider))
                .size(13.0)
                .strong()
                .color(text_color);
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 深浅两套配色都要保证文字与卡片底色有足够对比度
    #[test]
    fn palettes_have_readable_contrast() {
        for dark in [false, true] {
            let p = Palette::of(dark);
            let text = p.text.to_array()[0] as i32;
            let card = p.card.to_array()[0] as i32;
            assert!(
                (text - card).abs() > 100,
                "卡片文字与底色对比度不足（dark={dark}）"
            );
            let muted = p.muted.to_array()[0] as i32;
            assert!(
                (muted - card).abs() > 60,
                "次要文字对比度不足（dark={dark}）"
            );
        }
    }
}
