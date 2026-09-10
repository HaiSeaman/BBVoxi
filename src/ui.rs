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
use tokio::sync::mpsc::UnboundedSender;

/// 应用图标（128×128 RGBA），用作窗口内的品牌标识
static ICON_WINDOW: &[u8] = include_bytes!("../assets/rgba128_window.bin");

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
        }
    }

    /// 窗口隐藏到托盘时取消快捷键捕捉，避免钩子一直处于暂停状态
    pub fn cancel_capture(&mut self) {
        if self.capturing {
            self.end_capture();
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.handle_capture(ui.ctx());
        let p = Palette::of(ui.visuals().dark_mode);
        apply_widget_style(ui, &p);

        // 先放底部固定操作条，中央区域再吃掉剩下的空间（egui 的面板顺序要求）
        egui::Panel::bottom("bbvoxi_actions")
            .frame(
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(Margin::symmetric(16, 12)),
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
                        ui.add_space(14.0);
                        self.service_card(ui, &p);
                        ui.add_space(8.0);
                        self.hotkey_card(ui, &p);
                        ui.add_space(8.0);
                        self.options_card(ui, &p);
                        ui.add_space(8.0);
                        self.result_card(ui, &p);
                        ui.add_space(8.0);
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
            (p.warn, "录制中".to_string())
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
            egui::ComboBox::from_id_salt("provider")
                .selected_text(self.edit.provider.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for provider in Provider::ALL {
                        ui.selectable_value(&mut self.edit.provider, provider, provider.label());
                    }
                });
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
                            .size(11.5)
                            .color(p.muted),
                    );
                }
            });

            ui.add_space(6.0);
            ui.checkbox(
                &mut self.show_keys,
                RichText::new("显示密钥明文").size(13.0),
            );
        });
    }

    fn hotkey_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(p, ui, |ui, p| {
            section_title(ui, p, "快捷键");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if self.capturing {
                    ui.label(RichText::new("请按下新的组合键…").size(14.0).color(p.warn));
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
                        self.begin_capture();
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
                    "支持 Ctrl / Alt / Shift + 字母、数字、F1~F12 或空格；按下时会拦截该组合键",
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
            ui.checkbox(
                &mut self.edit.options.live_typing,
                RichText::new("边说话边打字（实时输入）").size(13.5),
            );
            ui.label(
                RichText::new("识别修正时自动回退重打；关掉则只在松手后一次性输入")
                    .size(11.5)
                    .color(p.muted),
            );
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.edit.options.auto_punctuation,
                RichText::new("自动添加标点").size(13.5),
            );
            ui.checkbox(
                &mut self.edit.options.smooth,
                RichText::new("口语顺滑（去除重复与语气词）").size(13.5),
            );
        });
    }

    /// 开机自启：勾选立即写注册表（失败会回滚勾选）
    fn autostart_checkbox(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let before = self.autostart;
        ui.checkbox(&mut self.autostart, RichText::new("开机自启").size(12.5));
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
        let _ = p;
    }

    fn result_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let snap = self.shared.snapshot();
        card(p, ui, |ui, p| {
            section_title(ui, p, "最近识别");
            ui.add_space(8.0);
            if snap.recording {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").size(12.0).color(p.danger));
                    ui.label(
                        RichText::new(format!("录音中 {}", clock(snap.elapsed())))
                            .size(13.0)
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
                        .size(11.5)
                        .color(p.muted),
                );
            }
        });
    }

    // —— 底部操作条 ——

    fn actions_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let recording = self.shared.snapshot().recording;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !recording,
                    egui::Button::new(RichText::new("测试识别（5 秒）").size(13.5).color(p.text))
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
            // 右侧放开机自启开关，操作条一行解决，不占卡片空间
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let color = self.status.as_ref().map(|(ok, _)| *ok);
                let msg = self.status.as_ref().map(|(_, m)| m.clone());
                self.autostart_checkbox(ui, p);
                ui.add_space(6.0);
                if let Some(msg) = msg {
                    let c = if color.unwrap_or(true) {
                        p.ok
                    } else {
                        p.danger
                    };
                    ui.label(RichText::new(msg).size(12.0).color(c));
                }
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

    fn begin_capture(&mut self) {
        self.capturing = true;
        self.hotkey_error = None;
        self.paused.store(true, Ordering::Relaxed);
    }

    fn end_capture(&mut self) {
        self.capturing = false;
        self.paused.store(false, Ordering::Relaxed);
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
        ctx.request_repaint();
        let (key, mods) = ctx.input(|i| {
            let key = i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key, pressed: true, ..
                } => Some(*key),
                _ => None,
            });
            (key, i.modifiers)
        });
        let Some(key) = key else { return };
        if key == egui::Key::Escape {
            self.end_capture();
            return;
        }
        let Some(vk) = egui_key_to_vk(key) else {
            self.hotkey_error = Some("该按键暂不支持，请用字母、数字、F1~F12 或空格".into());
            return;
        };
        let candidate = Hotkey {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            win: false,
            vk,
        };
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
        ui.label(RichText::new(label).size(12.0).color(p.muted));
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
        .inner_margin(Margin::symmetric(16, 12))
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

/// egui 按键 → Windows 虚拟键码（覆盖常用键）
fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    use egui::Key;
    let vk = match key {
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Tab => 0x09,
        Key::Backspace => 0x08,
        Key::Backtick => 0xC0,
        Key::Minus => 0xBD,
        Key::Equals => 0xBB,
        Key::OpenBracket => 0xDB,
        Key::CloseBracket => 0xDD,
        Key::Backslash => 0xDC,
        Key::Semicolon => 0xBA,
        Key::Quote => 0xDE,
        Key::Comma => 0xBC,
        Key::Period => 0xBE,
        Key::Slash => 0xBF,
        _ => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_covers_letters_digits_and_function_keys() {
        assert_eq!(egui_key_to_vk(egui::Key::A), Some(0x41));
        assert_eq!(egui_key_to_vk(egui::Key::Num1), Some(0x31));
        assert_eq!(egui_key_to_vk(egui::Key::F9), Some(0x78));
        assert_eq!(egui_key_to_vk(egui::Key::Space), Some(0x20));
        assert_eq!(egui_key_to_vk(egui::Key::Backtick), Some(0xC0));
        assert_eq!(egui_key_to_vk(egui::Key::ArrowDown), None);
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
