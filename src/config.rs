//! 配置模型与持久化。三家 API 地址写死在此，用户只填凭据与模型。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// —— 写死的官方接口地址（2026-09-10 核实自各家官方文档）——
pub const QWEN_URL: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference";
pub const QWEN_MODEL: &str = "qwen-audio-3.0-asr-flash-streaming";
pub const DOUBAO_URL_STREAM: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
pub const DOUBAO_URL_NOSTREAM: &str =
    "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream";
pub const DOUBAO_RESOURCE: &str = "volc.seedasr.sauc.duration";
pub const TENCENT_URL: &str = "wss://asr.cloud.tencent.com/asr/v2";
pub const TENCENT_ENGINES: [&str; 2] = ["Hy-ASR-3.0-preview", "16k_zh_en"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[serde(alias = "aliyun")]
    Qwen,
    Doubao,
    Tencent,
}

impl Default for Provider {
    fn default() -> Self {
        Provider::Qwen
    }
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            auto_punctuation: true,
            smooth: true,
            live_typing: true,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            provider: Provider::default(),
            hotkey: "ctrl+1".into(),
            qwen: QwenConfig::default(),
            doubao: DoubaoConfig::default(),
            tencent: TencentConfig::default(),
            options: Options::default(),
        }
    }
}

fn dir() -> Result<PathBuf> {
    dirs::config_dir().context("无法定位系统配置目录")
}

/// 返回 (配置, 是否首次运行即配置文件不存在)
pub fn load_or_default() -> Result<(Config, bool)> {
    let file = dir()?.join("BBVoxi").join("config.json");
    if !file.exists() {
        return Ok((Config::default(), true));
    }
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("读取配置失败: {}", file.display()))?;
    // ponytail: 配置损坏时回退默认值，不让用户被一个坏文件卡死
    match serde_json::from_str::<Config>(&text) {
        Ok(cfg) => Ok((cfg, false)),
        Err(e) => {
            eprintln!("配置文件解析失败，已回退默认配置: {e}");
            Ok((Config::default(), false))
        }
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
        let tmp = path.with_extension("json.tmp");
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

    #[test]
    fn doubao_endpoint_follows_accuracy_mode() {
        let mut d = DoubaoConfig::default();
        assert_eq!(d.endpoint(), DOUBAO_URL_STREAM);
        d.high_accuracy = true;
        assert_eq!(d.endpoint(), DOUBAO_URL_NOSTREAM);
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
        assert!(
            !path.with_extension("json.tmp").exists(),
            "保存成功后不该留下临时文件"
        );
        let before = std::fs::read_to_string(&path).unwrap();

        // 制造写入失败：在临时文件该在的位置放一个同名目录，写它必然失败
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        let broken = Config {
            provider: Provider::Tencent,
            ..Default::default()
        };
        assert!(broken.save_to(&path).is_err(), "写临时文件失败时必须如实报错");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, before, "保存失败时旧配置必须原封不动");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
