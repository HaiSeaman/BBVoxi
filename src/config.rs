//! 配置模型与持久化。三家 API 地址写死在此，用户只填凭据与模型。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// —— 写死的官方接口地址（2026-09-10 核实自各家官方文档）——
pub const QWEN_URL: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference";
/// 千问当前默认模型（3.1：官方最新流式识别模型，新增方言保留、近场 VAD 等能力）。
pub const QWEN_MODEL: &str = "qwen-audio-3.1-asr-flash-streaming";
/// 旧默认模型（3.0）。只用于配置迁移：老配置里存的还是它时自动升级到 3.1；
/// 主人自己改过的其他模型名（如 fun-asr-realtime）原样保留。
const QWEN_MODEL_LEGACY: &str = "qwen-audio-3.0-asr-flash-streaming";
pub const DOUBAO_URL_STREAM: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
pub const DOUBAO_URL_NOSTREAM: &str =
    "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream";
pub const DOUBAO_RESOURCE: &str = "volc.seedasr.sauc.duration";
pub const TENCENT_URL: &str = "wss://asr.cloud.tencent.com/asr/v2";
pub const TENCENT_ENGINES: [&str; 2] = ["Hy-ASR-3.0-preview", "16k_zh_en"];

/// 识别语言选项（内部代码 → 界面显示名），`Options.language` 存代码。
/// 三家落实方式不同：豆包映射官方 language 参数（并切整句端点）、
/// 腾讯映射语言引擎、千问模型自身自动检测（无需参数）。
pub const LANGUAGES: [(&str, &str); 10] = [
    ("auto", "自动"),
    ("zh", "中文"),
    ("en", "英语"),
    ("yue", "粤语"),
    ("ja", "日语"),
    ("ko", "韩语"),
    ("fr", "法语"),
    ("de", "德语"),
    ("ru", "俄语"),
    ("es", "西班牙语"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    #[serde(alias = "aliyun")]
    Qwen,
    Doubao,
    Tencent,
}

/// 界面主题：跟随系统的深浅色，或固定其中一种。
///
/// 序列化成小写字符串（"system" / "light" / "dark"），配置文件里一眼能看懂。
/// `Config` 整体带 `#[serde(default)]`，所以老配置文件里没有这个字段时
/// 自动落回「跟随系统」—— 不需要版本迁移，主人升级后行为不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "dark")]
    Dark,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Qwen, Provider::Doubao, Provider::Tencent];
    pub fn label(&self) -> &'static str {
        match self {
            Provider::Qwen => "千问（阿里云百炼）",
            Provider::Doubao => "豆包（火山引擎）",
            Provider::Tencent => "腾讯云",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QwenConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

impl Default for QwenConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: QWEN_MODEL.into(),
            base_url: QWEN_URL.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DoubaoConfig {
    pub api_key: String,
    pub resource_id: String,
    /// true = bigmodel_nostream（整句二次识别，准确率优先）；false = bigmodel_async（实时优先）
    pub high_accuracy: bool,
}

impl Default for DoubaoConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            resource_id: DOUBAO_RESOURCE.into(),
            high_accuracy: false,
        }
    }
}

impl DoubaoConfig {
    pub fn endpoint(&self) -> &'static str {
        if self.high_accuracy {
            DOUBAO_URL_NOSTREAM
        } else {
            DOUBAO_URL_STREAM
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TencentConfig {
    pub app_id: String,
    pub secret_id: String,
    pub secret_key: String,
    pub engine_model_type: String,
}

impl Default for TencentConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            secret_id: String::new(),
            secret_key: String::new(),
            engine_model_type: TENCENT_ENGINES[0].into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub auto_punctuation: bool,
    pub smooth: bool,
    /// 边说话边打字（识别中间结果实时写入目标程序，被修正时自动回退重打）
    pub live_typing: bool,
    /// 逐字注入被目标程序拒掉时，改用剪贴板粘贴兜底
    pub clipboard_fallback: bool,
    /// 识别结果总留一份到剪贴板（成功输入也留）
    pub keep_on_clipboard: bool,
    /// 个人词典（热词），一行一个词：人名、品牌名、术语等会被优先识别。
    /// 千问走即时热词 + 上下文增强；豆包走请求级热词；腾讯云暂不支持。
    pub hotwords: String,
    /// 识别语言（"auto" = 跟随各家默认的中英自动检测）。
    /// 非自动时：豆包传官方 language 参数（自动切整句端点）、腾讯切对应引擎、
    /// 千问模型自身多语种自动检测（无需参数）。
    pub language: String,
    /// 录音用哪台麦克风：存设备的唯一编号（形如
    /// `wasapi:{0.0.1.00000000}.{...}`，见 `audio::MicDevice`）。
    /// **空字符串 = 跟随系统默认设备** —— 老配置文件里没有这一项，读出来就是空，
    /// 行为与升级前完全一样，所以不需要版本迁移。
    /// 界面上显示的是 Windows 那个好认的名字，编号只在文件里存着。
    pub mic_device: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            auto_punctuation: true,
            smooth: true,
            live_typing: true,
            // 兜底默认开。
            clipboard_fallback: true,
            // 默认**开**（1.3.2 起改的，老配置会被迁移过来）。
            //
            // 改它的原因：这个开关叫"识别结果总留一份到剪贴板"，而主人真正需要它
            // 兜的就是"字到底有没有打进目标程序"——**这件事我们无法核实**：
            // `SendInput` 只报告"事件已入队"，目标程序完全可以收下然后丢掉；
            // 实测（Chrome/Electron 这类目标）连插入符都不给，看不出任何痕迹。
            // 所以唯一可靠的兜底就是"永远留一份"：主人随时 Ctrl+V 都还在。
            // 代价是主人原来复制的内容会被顶掉（这正是它的说明里写的），
            // 不想要这个代价的可以显式关掉。
            keep_on_clipboard: true,
            // 个人词典默认空（不传热词，行为与升级前完全一致）
            hotwords: String::new(),
            // 语言默认"自动"：不传 language 参数，行为与升级前完全一致
            language: "auto".into(),
            // 麦克风默认空 = 跟随系统默认设备（与升级前一致）
            mic_device: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub provider: Provider,
    pub hotkey: String,
    pub qwen: QwenConfig,
    pub doubao: DoubaoConfig,
    pub tencent: TencentConfig,
    pub options: Options,
    pub theme: ThemeMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            provider: Provider::default(),
            // 默认「Ctrl + Win」：这个组合在 Windows 上几乎没有程序占用，
            // 又是纯修饰键组合（不需要再记一个主键），按住说话最顺手。
            // 左右 Ctrl 都认（见 hotkey 模块的说明）。
            hotkey: "ctrl+win".into(),
            qwen: QwenConfig::default(),
            doubao: DoubaoConfig::default(),
            tencent: TencentConfig::default(),
            options: Options::default(),
            theme: ThemeMode::default(),
        }
    }
}

/// 老版本的默认快捷键（Ctrl + `）。
///
/// 用它来判断"主人从没改过快捷键"：只要配置里的值解析出来还是这个组合，
/// 就说明是旧版留下的默认值。比字符串比较稳（大小写、写法差异都能认出来）。
fn legacy_default_hotkey() -> crate::hotkey::Hotkey {
    crate::hotkey::Hotkey {
        ctrl: true,
        alt: false,
        shift: false,
        win: false,
        vk: 0xC0,
    }
}

/// 把旧默认值升级成新版默认值（Ctrl + Win），返回是否改动了配置。
///
/// 为什么必须有这一步：只改 `Config::default()` 救不了**已经存在的配置文件** ——
/// 程序启动是"文件在就用文件里的值"，于是主人升级后打开一看还是老快捷键，
/// 怎么改代码都没用。这是"我明明让你改了、你就是没改"的直接原因。
/// 主人自己改过的任何组合都会原样保留，只有"一次都没动过"的旧默认值才升级。
fn migrate_legacy_default_hotkey(cfg: &mut Config) -> bool {
    let Ok(old) = crate::hotkey::parse(&cfg.hotkey) else {
        return false;
    };
    if old != legacy_default_hotkey() {
        return false;
    }
    cfg.hotkey = Config::default().hotkey;
    crate::log::log("旧默认快捷键 Ctrl+` 已升级为 Ctrl+Win（可在设置窗口里改）");
    true
}

/// 当前配置版本。引入"必须改老配置"的行为变更时就 +1，并在 [`migrate`] 里补一步。
const CURRENT_VERSION: u32 = 3;

/// 把读到的配置升级到当前版本，返回是否改动了配置（改了就要写回）。
///
/// 为什么必须有这一步：只改 `Default` 救不了**已经存在的配置文件** ——
/// 启动逻辑是"文件在就用文件里的值"，于是主人升级后一看，行为还是老样子。
/// （`migrate_legacy_default_hotkey` 当初就是为这个加的。）
///
/// 1 → 2：`keep_on_clipboard` 老默认是关的，而它兜的恰恰是"字没打进目标程序"
/// 这一档 —— 那一档**无法核实**（`SendInput` 只报告事件入队成功，目标程序收下
/// 再丢掉我们完全看不见；Chrome/Electron 这类目标连插入符都不给，实测过），
/// 所以唯一可靠的兜底就是"永远留一份"。改为默认开。
///
/// 2 → 3：千问默认模型 3.0 → 3.1（3.1 支持近场 VAD、去语气词等新参数）。
/// 只升"从没动过"的旧默认值，主人自己填的模型名原样保留（同快捷键迁移的套路）。
fn migrate(cfg: &mut Config) -> bool {
    if cfg.version >= CURRENT_VERSION {
        return false;
    }
    // **每一步只在自己那一段版本区间里跑**。
    //
    // 为什么必须分区间：以前只有上面一道总闸（`version < CURRENT_VERSION` 就把
    // 三步全跑一遍），于是 1.3.x 的用户（配置里 version = 2）升级到本版时，
    // 「1 → 2」那两步会被**再跑一次** —— 主人自己关掉的「结果总留一份到剪贴板」
    // 被强行打开（从此每说一句都顶掉剪贴板里刚复制的东西）、自己设成 Ctrl+`
    // 的快捷键被改回 Ctrl+Win，而且界面上一个字都不说。
    // 这正是这些注释自己警告过的"升级顺手改掉主人设置"。
    if cfg.version < 2 {
        // 旧默认值 Ctrl+` → Ctrl+Win。**只看升版本这一次**（在下面这个区间里）：
        // 以前它每次启动都跑，于是主人后来自己把快捷键改成 Ctrl+` 时，下次启动
        // 就被悄悄改回 Ctrl+Win —— `migration_leaves_user_hotkeys_alone` 的清单里
        // 恰好漏了这一项，所以那个毛病当初一直没被抓住。
        migrate_legacy_default_hotkey(cfg);
        // 1 → 2：`keep_on_clipboard` 老默认是关的，而它兜的恰恰是"字没打进目标程序"
        // 这一档 —— 那一档**无法核实**（`SendInput` 只报告事件入队成功，目标程序
        // 收下再丢掉我们完全看不见；Chrome/Electron 这类目标连插入符都不给，
        // 实测过），所以唯一可靠的兜底就是"永远留一份"。改为默认开。
        //
        // 老配置里这个值是"从没动过"留下的默认 false，还是主人真的关过 ——
        // 两者在文件里长得一模一样，分不出来。按"漏掉主人说的话"比"覆盖一次
        // 剪贴板"更糟来取舍：统一打开。不想要的在设置里关掉即可，
        // 关掉之后 version 已经是 2，下次启动不会再被打开。
        if !cfg.options.keep_on_clipboard {
            cfg.options.keep_on_clipboard = true;
            crate::log::log("配置升级：识别结果改为默认留一份到剪贴板（可在设置里关掉）");
        }
    }
    // 2 → 3：千问旧默认模型 3.0 → 3.1。字符串精确匹配旧默认值：
    // 主人自己改过的模型（哪怕只差一个字符）都不动。
    // 这一步对**所有**低于当前版本的配置都要跑（从 version 1 一步跨上来的也要）。
    if cfg.qwen.model == QWEN_MODEL_LEGACY {
        cfg.qwen.model = QWEN_MODEL.into();
        crate::log::log("配置升级：千问默认模型已从 3.0 升级到 3.1");
    }
    cfg.version = CURRENT_VERSION;
    // 版本号本身就是要落盘的改动，所以这里一律返回 true（幂等靠上面那句早退保证）
    true
}

fn dir() -> Result<PathBuf> {
    dirs::config_dir().context("无法定位系统配置目录")
}

/// 原子写用的临时文件路径：文件名带上进程 id。
///
/// 为什么必须带 pid：以前固定叫 `config.json.tmp`，两个实例同时保存（或同一实例里
/// 启动时的迁移写回与设置窗保存撞在一起）会互相覆盖这一个临时文件，可能把对方写到
/// 一半的半截内容改名成正式配置 —— 配置损坏、API Key 丢失。带上 pid 后各写各的。
fn temp_path(path: &std::path::Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("config");
    path.with_file_name(format!("{stem}.{}.tmp", std::process::id()))
}

/// 返回 (配置, 是否首次运行即配置文件不存在)
pub fn load_or_default() -> Result<(Config, bool)> {
    Ok(load_from(&dir()?.join("BBVoxi").join("config.json")))
}

/// 从指定文件读配置（`load_or_default` 的实现，**单独抽出来是为了能单测**：
/// 默认路径来自系统目录，测试里没法往里放坏文件）。
///
/// 返回 (配置, 是否首次运行即配置文件不存在)。
/// 语义注意：只有"文件不存在"才算首次运行，**读失败不算** —— 拿读失败当首次运行，
/// 会让界面显示成"全新安装"，主人以为自己的配置被清了。
fn load_from(file: &std::path::Path) -> (Config, bool) {
    if !file.exists() {
        return (Config::default(), true);
    }
    // 读失败（文件被写坏成非法 UTF-8、权限被拒、被独占锁住等）也回退默认值，而不是把
    // 错误往上抛：main 里的 `?` 传播出去后，release 版是 windows_subsystem="windows"，
    // 既没有控制台也不会写日志，主人只看到"双击毫无反应"，连哪里坏了都不知道。
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            backup_unreadable(file, &format!("读取失败：{e}"));
            return (Config::default(), false);
        }
    };
    // ponytail: 配置损坏时回退默认值，不让用户被一个坏文件卡死
    match serde_json::from_str::<Config>(&text) {
        Ok(mut cfg) => {
            if migrate(&mut cfg) {
                if let Err(e) = cfg.save() {
                    // 写回失败不影响这次启动：内存里已经是新值，下次启动会再试一遍。
                    // 但必须留下痕迹 —— 否则主人下次开还是老样子，又会以为"没改"。
                    crate::log::log(format!("升级配置后写回失败（下次启动会再试一遍）: {e}"));
                }
            }
            (cfg, false)
        }
        Err(e) => {
            // 不能用 eprintln!：release 下 windows_subsystem="windows"，没有控制台，
            // 这一行会直接消失。这里不是"恢复默认"就完了 —— 主人需要知道凭据为什么没了。
            backup_unreadable(file, &format!("解析失败：{e}"));
            (Config::default(), false)
        }
    }
}

/// 读不出来 / 解析不了时，**先把原文件另存一份**，再退回默认值。
///
/// 为什么必须有这一步（审查时发现的真实数据丢失路径）：读失败可能是**临时**的
/// （文件被别的程序独占、杀软正在扫）。退回默认值之后，主人只要做了什么会写盘的
/// 操作（改个开关点保存，甚至只切一下主题就会立即落盘），就会把那个其实好好的
/// 配置文件覆盖成空配置 —— API Key 全部永久丢失，而且毫无提示。
/// 另存一份之后，最坏也能把它改回 `config.json` 救回来。
fn backup_unreadable(file: &std::path::Path, why: &str) {
    let backup = file.with_extension("json.bad");
    let saved = std::fs::copy(file, &backup);
    match saved {
        Ok(_) => crate::log::log(format!(
            "配置文件无法使用（{why}）：原文件已另存为 {}，本次先用默认配置启动。\
             想恢复的话把它改回 config.json 即可。",
            backup.display()
        )),
        Err(e) => crate::log::log(format!(
            "配置文件无法使用（{why}），而且另存备份也失败了（{e}）：\
             **在确认之前先别点保存**，否则原文件会被默认配置覆盖。"
        )),
    }
}

impl Config {
    pub fn save(&self) -> Result<()> {
        self.save_to(&dir()?.join("BBVoxi").join("config.json"))
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // 原子写：先写同目录下的临时文件，成功后再改名覆盖目标。
        // 直接覆盖写的话，写到一半崩溃/断电会留下半截坏文件，下次启动只能
        // 静默回退默认配置（凭据全丢）。同卷内的改名是原子的：要么旧文件，
        // 要么完整的新文件，不会出现"半截"状态。
        let tmp = temp_path(path);
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 当前服务商是否已填好可用凭据（M3 连通性测试与 M2 录音前校验用）
    pub fn credentials_ready(&self) -> bool {
        match self.provider {
            Provider::Qwen => {
                !self.qwen.api_key.trim().is_empty() && !self.qwen.model.trim().is_empty()
            }
            Provider::Doubao => {
                !self.doubao.api_key.trim().is_empty() && !self.doubao.resource_id.trim().is_empty()
            }
            Provider::Tencent => {
                !self.tencent.app_id.trim().is_empty()
                    && !self.tencent.secret_id.trim().is_empty()
                    && !self.tencent.secret_key.trim().is_empty()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_hardcoded_and_roundtrip() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(cfg.qwen.base_url, QWEN_URL);
        assert_eq!(cfg.qwen.model, QWEN_MODEL);
        assert_eq!(cfg.doubao.resource_id, DOUBAO_RESOURCE);
        assert_eq!(cfg.tencent.engine_model_type, "Hy-ASR-3.0-preview");
    }

    /// 主题字段对老配置文件必须是纯增量：文件里没有这一项就落回「跟随系统」，
    /// 主人升级后界面行为一个字都不变（不需要版本迁移）。
    #[test]
    fn theme_defaults_to_system_and_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&ThemeMode::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&ThemeMode::Light).unwrap(), "\"light\"");
        assert_eq!(serde_json::to_string(&ThemeMode::Dark).unwrap(), "\"dark\"");
        // 老配置文件：没有任何 theme 字段
        let old: Config = serde_json::from_str(
            "{\"version\":2,\"provider\":\"qwen\",\"hotkey\":\"ctrl+win\",\
             \"qwen\":{},\"doubao\":{},\"tencent\":{},\"options\":{}}",
        )
        .expect("老配置文件必须能读进来");
        assert_eq!(old.theme, ThemeMode::System, "缺字段时默认跟随系统");
    }

    /// 默认快捷键必须是「Ctrl + Win」，而且必须真的能解析出来 ——
    /// 默认值写错一个词，首次启动就会在日志里报"快捷键无法解析"并悄悄回退。
    #[test]
    fn default_hotkey_is_ctrl_win() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey, "ctrl+win");
        let hk = crate::hotkey::parse(&cfg.hotkey).expect("默认快捷键必须能解析");
        assert!(hk.ctrl && hk.win && !hk.alt && !hk.shift);
        assert!(hk.is_pure_modifiers(), "默认组合不该带主键");
        assert_eq!(hk.display(), "Ctrl + Win");
        assert_eq!(hk.to_config(), cfg.hotkey, "存回配置还要是同一个写法");
    }

    /// 旧版默认值 Ctrl+` 必须自动升级成 Ctrl+Win。
    ///
    /// 这是主人"改了默认值却看不到变化"的根因：程序用已存在的配置文件里的值，
    /// 改代码里的默认值对它无效。这条测试守住升级这件事真的会发生。
    #[test]
    fn migrates_legacy_default_hotkey_to_ctrl_win() {
        let mut cfg = Config {
            hotkey: "ctrl+`".into(),
            ..Config::default()
        };
        assert!(
            migrate_legacy_default_hotkey(&mut cfg),
            "旧默认值必须被升级"
        );
        assert_eq!(cfg.hotkey, "ctrl+win");
        assert!(
            !migrate_legacy_default_hotkey(&mut cfg),
            "已经升级过就不能再动，否则每次启动都写一遍配置"
        );
    }

    /// 主人自己改过的组合，一个都不许动。
    ///
    /// 升级逻辑最容易犯的错就是"顺手把别人的设置也改了"：那样主人配好的
    /// Alt+Q 会在某次启动后悄悄变成 Ctrl+Win，而界面上不会有任何提示。
    #[test]
    fn migration_leaves_user_hotkeys_alone() {
        for user in [
            "ctrl+alt+a",
            "f9",
            "shift+win",
            "ctrl+shift",
            "alt+1",
            "ctrl+win",
        ] {
            let mut cfg = Config {
                hotkey: user.into(),
                ..Config::default()
            };
            assert!(
                !migrate_legacy_default_hotkey(&mut cfg),
                "{user} 不该被改写"
            );
            assert_eq!(cfg.hotkey, user, "{user} 必须原样保留");
        }
    }

    /// 回归（主人自己改的快捷键会被悄悄改回去）：**已经是当前版本的配置**，
    /// 哪怕快捷键正好等于旧版默认值（Ctrl+反引号键），也一个字都不许动。
    ///
    /// 旧实现每次都跑热键迁移，于是"版本已经是 2、主人明确设成 Ctrl+`"的配置
    /// 下次启动会被改回 Ctrl+Win —— 主人只会觉得"我改了它自己变回去了"。
    /// 这条与 `migration_leaves_user_hotkeys_alone` 的区别：那条测的是纯函数，
    /// 这条测的是"版本号决定要不要迁移"这层判断。
    #[test]
    fn migration_never_overwrites_a_current_config() {
        let mut cfg = Config {
            version: CURRENT_VERSION,
            hotkey: "ctrl+`".into(),
            ..Config::default()
        };
        assert!(!migrate(&mut cfg), "已是当前版本就不该动它");
        assert_eq!(cfg.hotkey, "ctrl+`", "主人自己设的快捷键被改回默认值了");
    }

    /// 升级后的配置要能真的存下来、读回来还是新值（否则只是内存里好看）。
    #[test]
    fn migrated_hotkey_survives_a_save_and_reload() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_migrate_{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");
        let mut cfg = Config {
            hotkey: "ctrl+`".into(),
            ..Config::default()
        };
        assert!(migrate_legacy_default_hotkey(&mut cfg));
        cfg.save_to(&path).unwrap();
        let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.hotkey, "ctrl+win");
        let hk = crate::hotkey::parse(&back.hotkey).unwrap();
        assert!(hk.ctrl && hk.win && hk.is_pure_modifiers());
    }

    /// 回归（千问 3.1 升级）：老配置里存的旧默认模型（3.0）必须自动升到 3.1。
    /// 与快捷键迁移同套路：只认"从没动过"的精确旧默认值。
    #[test]
    fn migrates_legacy_default_qwen_model_to_31() {
        let mut cfg = Config {
            version: 2,
            qwen: QwenConfig {
                model: QWEN_MODEL_LEGACY.into(),
                ..QwenConfig::default()
            },
            ..Config::default()
        };
        assert!(migrate(&mut cfg), "2 → 3 必须报告配置已改动");
        assert_eq!(cfg.qwen.model, QWEN_MODEL, "旧默认模型必须升到 3.1");
        assert_eq!(cfg.version, CURRENT_VERSION);
        // 已是当前版本：再跑一遍什么都不能动（幂等）
        assert!(!migrate(&mut cfg));
    }

    /// 主人自己填的模型名（如 fun-asr-realtime），哪怕只差一个字符，都不许动。
    #[test]
    fn migration_leaves_custom_qwen_models_alone() {
        let mut cfg = Config {
            version: 2,
            qwen: QwenConfig {
                model: "fun-asr-realtime".into(),
                ..QwenConfig::default()
            },
            ..Config::default()
        };
        assert!(migrate(&mut cfg), "版本号本身要写回（2 → 3）");
        assert_eq!(
            cfg.qwen.model, "fun-asr-realtime",
            "主人自己改过的模型必须原样保留"
        );
    }

    /// 回归（升级顺手改掉主人自己的设置）：停在 version 2 的配置（1.3.x 用户）
    /// 升到 1.4.0 时，**1→2 那两步绝不能再跑一遍**。
    ///
    /// 旧实现只有一道总闸（`version < CURRENT_VERSION` 就把三步全跑），于是
    /// 「1→2」的快捷键迁移与"强制打开留一份到剪贴板"被重复执行：主人自己关掉的
    /// 开关被重新打开（从此每说一句都顶掉剪贴板里刚复制的东西）、自己设成
    /// Ctrl+` 的快捷键被改回 Ctrl+Win，而界面上一句话都不说。
    /// 老的两条回归测试用的都是 `version: CURRENT_VERSION`，正好绕开真实的升级
    /// 路径，所以这个毛病一直没被抓到。
    #[test]
    fn migration_v2_leaves_user_choices_alone() {
        let mut cfg = Config {
            version: 2,
            hotkey: "ctrl+`".into(),
            ..Config::default()
        };
        cfg.options.keep_on_clipboard = false; // 主人在 1.3.3 里明确关掉的
        assert!(migrate(&mut cfg), "版本号本身要写回（2 → 3）");
        assert!(
            !cfg.options.keep_on_clipboard,
            "主人明确关掉的「留一份到剪贴板」被升级重新打开了"
        );
        assert_eq!(cfg.hotkey, "ctrl+`", "主人自己设的快捷键被改回默认值了");
        assert_eq!(cfg.version, CURRENT_VERSION);
    }

    /// 升级到当前版本之后，1→2 的两步再也不会动主人后来改的设置（幂等）
    #[test]
    fn migration_never_repeats_the_old_steps() {
        let mut cfg = Config {
            version: CURRENT_VERSION,
            hotkey: "ctrl+`".into(),
            ..Config::default()
        };
        cfg.options.keep_on_clipboard = false;
        assert!(!migrate(&mut cfg), "已是当前版本就不该动它");
        assert!(!cfg.options.keep_on_clipboard);
        assert_eq!(cfg.hotkey, "ctrl+`");
    }

    /// 新增的 hotwords / language / mic_device 字段对老配置是纯增量：
    /// 文件里没有就落回默认（空词表 / auto / 跟随系统默认麦克风），
    /// 行为与升级前完全一致。
    #[test]
    fn new_option_fields_default_for_old_configs() {
        let old: Config = serde_json::from_str(
            "{\"version\":2,\"provider\":\"qwen\",\"hotkey\":\"ctrl+win\",\
             \"qwen\":{},\"doubao\":{},\"tencent\":{},\"options\":{},\"theme\":\"system\"}",
        )
        .expect("老配置文件必须能读进来");
        assert_eq!(old.options.hotwords, "");
        assert_eq!(old.options.language, "auto");
        assert_eq!(
            old.options.mic_device, "",
            "老配置没有「麦克风」这一项时必须落回跟随系统默认设备"
        );
        // 默认值本身
        assert_eq!(Options::default().hotwords, "");
        assert_eq!(Options::default().language, "auto");
        assert_eq!(Options::default().mic_device, "");
    }

    /// 选中的麦克风必须真的存下来、读回来还是同一台（否则主人每次开机都要重选）。
    /// 存的是设备编号而不是名字：两台同型号麦克风名字一模一样，只有编号分得开。
    #[test]
    fn selected_microphone_survives_a_save_and_reload() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_mic_{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");
        let cfg = Config {
            options: Options {
                mic_device: "wasapi:{0.0.1.00000000}.{92bdc9f1-bcf9-4c64-b02e-dc429cb283f9}".into(),
                ..Options::default()
            },
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.options.mic_device, cfg.options.mic_device);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doubao_endpoint_follows_accuracy_mode() {
        let mut d = DoubaoConfig::default();
        assert_eq!(d.endpoint(), DOUBAO_URL_STREAM);
        d.high_accuracy = true;
        assert_eq!(d.endpoint(), DOUBAO_URL_NOSTREAM);
    }

    /// 回归（配置读坏 / 被临时锁住时，密钥会被默认值盖掉 —— 再也找不回来）：
    /// 只要读不出来或解析不了，就必须**先把原文件另存一份**再退回默认值。
    ///
    /// 这条路径很阴：读失败可能是临时的（文件被独占、杀软在扫），退回默认值之后
    /// 主人随便点一下会写盘的东西（甚至只切个主题就会立即落盘），那个其实好好的
    /// 配置文件就被空配置覆盖了 —— API Key 永久丢失，界面上还什么都不说。
    #[test]
    fn unreadable_config_is_backed_up_before_falling_back() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_badcfg_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 1) 写坏成非法 JSON：原文件必须被另存为 config.json.bad
        let broken = r#"{"provider":"qwen","qwen":{"api_key":"sk-主人的密钥"#; // 截断的 JSON
        std::fs::write(&path, broken).unwrap();
        let (cfg, first_run) = load_from(&path);
        assert!(!first_run, "读失败不算首次运行（界面不该显示成全新安装）");
        assert_eq!(cfg, Config::default(), "读不出来时先用默认值启动");
        let backup = path.with_extension("json.bad");
        assert!(
            backup.exists(),
            "必须留下备份，否则主人的密钥再也找不回来"
        );
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            broken,
            "备份必须是原文件**一字不动**的内容"
        );
        assert!(path.exists(), "原文件也不能删（主人可能还要自己看）");

        // 2) 好文件：照常读出来，而且不该产生备份
        let _ = std::fs::remove_file(&backup);
        let good = Config {
            qwen: QwenConfig {
                api_key: "sk-正常的".into(),
                ..QwenConfig::default()
            },
            ..Config::default()
        };
        good.save_to(&path).unwrap();
        let (back, first_run) = load_from(&path);
        assert!(!first_run);
        assert_eq!(back.qwen.api_key, "sk-正常的");
        assert!(!backup.exists(), "正常读取不该产生备份文件");

        // 3) 文件不存在 = 首次运行
        let _ = std::fs::remove_file(&path);
        let (_, first_run) = load_from(&path);
        assert!(first_run, "文件不存在才算首次运行");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_dir_and_roundtrips() {
        let path = std::env::temp_dir()
            .join("bbvoxi_selftest")
            .join("config.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let cfg = Config {
            provider: Provider::Doubao,
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&text).unwrap(), cfg);
    }

    #[test]
    fn missing_fields_in_old_config_fall_back_to_defaults() {
        let back: Config = serde_json::from_str(r#"{"provider":"tencent"}"#).unwrap();
        assert_eq!(back.provider, Provider::Tencent);
        assert!(back.options.auto_punctuation);
        assert!(back.options.smooth);
        assert!(!back.credentials_ready());
    }

    /// 老配置文件里已经删掉的字段（如 max_record_secs）不能导致加载失败
    #[test]
    fn removed_field_in_old_config_is_ignored() {
        let raw = r#"{"provider":"qwen","options":{"auto_punctuation":false,"smooth":true,"max_record_secs":55}}"#;
        let cfg: Config = serde_json::from_str(raw).expect("旧配置必须还能读");
        assert_eq!(cfg.provider, Provider::Qwen);
        assert!(!cfg.options.auto_punctuation);
    }

    /// 老配置文件里没有这两个新开关，必须能读、且拿到我们定的默认值。
    /// 两者默认都**开**，理由见 `Options::default` 的说明：
    /// 字到底有没有打进目标程序**无法核实**，只有"永远留一份"才兜得住 ——
    /// 主人报的"输入没进去、剪贴板里也找不到"就是这个默认值造成的。
    #[test]
    fn clipboard_options_default_for_old_configs() {
        let cfg: Config =
            serde_json::from_str(r#"{"options":{"live_typing":true}}"#).expect("旧配置必须还能读");
        assert!(cfg.options.clipboard_fallback, "兜底应默认开");
        assert!(cfg.options.keep_on_clipboard, "留一份应默认开");
    }

    /// 1 → 2 的迁移：老配置里 `keep_on_clipboard: false` 必须被打开，
    /// 否则主人升级后行为一点没变（他报的 bug 依旧）。同时 version 要写上去。
    #[test]
    fn migration_v1_turns_on_keep_on_clipboard() {
        let mut cfg = Config {
            version: 1,
            ..Config::default()
        };
        cfg.options.keep_on_clipboard = false; // 老默认值：关
        assert!(migrate(&mut cfg), "老版本必须被升级");
        assert!(cfg.options.keep_on_clipboard, "升级后应改为留一份");
        assert_eq!(cfg.version, CURRENT_VERSION);

        // 幂等：升级过之后不能再改，否则每次启动都写一遍配置
        assert!(!migrate(&mut cfg), "升级过一次就不该再改动");
    }

    /// 迁移过之后主人自己关掉的开关，绝不能被再次打开 ——
    /// 这是"升级逻辑顺手改掉主人设置"的现场，用户会当成程序不听话。
    #[test]
    fn migration_respects_a_later_explicit_choice() {
        let mut cfg = Config {
            version: CURRENT_VERSION,
            ..Config::default()
        };
        cfg.options.keep_on_clipboard = false;
        assert!(!migrate(&mut cfg), "已是当前版本就不该动它");
        assert!(
            !cfg.options.keep_on_clipboard,
            "主人升级后明确关掉的开关被重新打开了"
        );
    }

    /// 保存必须是"要么整体成功、要么旧文件原封不动"。
    ///
    /// 直接覆盖写的话，写到一半崩溃/断电会留下半截坏文件，下次启动只能静默
    /// 回退默认配置 —— 主人的 API Key 就这么没了。所以改成"先写临时文件、
    /// 再改名覆盖"，写临时文件失败时旧配置必须一个字都没动。
    #[test]
    fn failed_save_keeps_the_previous_config_intact() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_atomic_{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");
        let original = Config {
            provider: Provider::Qwen,
            ..Default::default()
        };
        original.save_to(&path).unwrap();
        assert!(!temp_path(&path).exists(), "保存成功后不该留下临时文件");
        let before = std::fs::read_to_string(&path).unwrap();

        // 制造写入失败：在临时文件该在的位置放一个同名目录，写它必然失败。
        // 临时路径必须用 temp_path 算（带进程 id）：否则造出的障碍位置和程序真正
        // 写的位置对不上，这条测试就退化成"永远通过"，再坏也发现不了。
        std::fs::create_dir(temp_path(&path)).unwrap();
        let broken = Config {
            provider: Provider::Tencent,
            ..Default::default()
        };
        assert!(
            broken.save_to(&path).is_err(),
            "写临时文件失败时必须如实报错"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, before, "保存失败时旧配置必须原封不动");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 临时文件名必须带上进程 id。
    ///
    /// 守的是"两个实例同时保存不会互相覆盖临时文件"：固定名字下，一方可能把另一方
    /// 写到一半的半截内容改名成正式配置（配置损坏、API Key 丢失）。
    #[test]
    fn temp_path_carries_process_id() {
        let path = std::path::Path::new("config.json");
        let tmp = temp_path(path);
        assert_eq!(
            tmp.file_name().unwrap().to_str().unwrap(),
            format!("config.{}.tmp", std::process::id())
        );
        assert_ne!(tmp, path, "临时文件不能就是目标文件本身（否则没有原子性）");
    }
}
