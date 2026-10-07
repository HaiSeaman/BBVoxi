//! 腾讯云实时语音识别（WebSocket）。
//! 地址已核实：wss://asr.cloud.tencent.com/asr/v2/{appid}?{参数}&signature=...
//! 鉴权是 AppID + SecretID + SecretKey 三元组，本地算 HMAC-SHA1 签名；引擎推荐 Hy-ASR-3.0-preview。

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use sha1::Sha1;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

use super::{pcm_bytes, Kind, Parsed};
use crate::config::TencentConfig;

pub struct Tencent;

impl Tencent {
    pub fn new() -> Self {
        Tencent
    }

    pub fn audio_messages(&mut self, pcm: &[i16]) -> Vec<Message> {
        vec![Message::binary(pcm_bytes(pcm))]
    }

    /// 腾讯规定用文本消息 {"type":"end"} 通知结束
    pub fn finish_messages(&mut self) -> Vec<Message> {
        vec![Message::text(r#"{"type":"end"}"#.to_string())]
    }

    pub fn parse(&mut self, msg: Message) -> Parsed {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(frame) => return super::parse_close("腾讯云", frame),
            _ => return Parsed::Ignored,
        };
        parse_json(&text)
    }
}

fn parse_json(text: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        // 坏帧不能整帧静默吞掉（同豆包/千问）：协议一变就是"等 8 秒超时 → 没有
        // 识别到内容"，日志里却什么都没有。只打前 16 个字符，不倒整帧。
        crate::log::log(format!(
            "腾讯云响应不是合法 JSON（前 16 字符：{}）",
            text.chars().take(16).collect::<String>()
        ));
        return Parsed::Ignored;
    };
    let code = v["code"].as_i64().unwrap_or(0);
    if code != 0 {
        let message = v["message"].as_str().unwrap_or("");
        return Parsed::Error(format!(
            "腾讯云返回错误 {code}：{}{}",
            message,
            tencent_hint(code)
        ));
    }

    // 缺字段按 0（"不是稳态/终态"）处理，**不能**拿 -1 这种值域外的哨兵当默认：
    // 那样"字段没来"和"来了个怪值"就混成同一个东西了（豆包那边踩过同样的坑）。
    let slice_type = v["result"]["slice_type"].as_i64().unwrap_or(0);
    let text = v["result"]["voice_text_str"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let is_final = v["final"].as_i64().unwrap_or(0) == 1;

    // 注意 `finished` 要跟着文本一起上报：腾讯会把 `final=1` 和最后一句的文本
    // 放进同一个包。若这里只判 slice_type，这一包就被当成普通文本，
    // 会话永远等不到结束 —— 主人松手后要白等满 8 秒超时。
    // `id: None`：腾讯的协议里没有句子编号，无法按编号去重（见 `Transcript`）。
    match slice_type {
        2 if !text.is_empty() => Parsed::Text {
            kind: Kind::Final,
            text,
            finished: is_final,
            id: None,
        },
        1 if !text.is_empty() => Parsed::Text {
            kind: Kind::Partial,
            text,
            finished: is_final,
            id: None,
        },
        // 首帧：腾讯会把 slice_type 报成 0（还不是稳态），但**已经带上了识别文本**。
        // 以前这里直接落到下面的 `_ if is_final => Finished` / `_ => Ignored`，
        // 这一整段文本就被丢掉了 —— 短音频（只有首帧就结束）的主人会看到
        // "没有识别到内容"。所以这里必须把文本补上：非终包按中间结果报，
        // 终包按定稿（finished=true）报，否则会话还得多等 8 秒超时。
        0 if !text.is_empty() => Parsed::Text {
            kind: if is_final { Kind::Final } else { Kind::Partial },
            text,
            finished: is_final,
            id: None,
        },
        _ if is_final => Parsed::Finished,
        _ => Parsed::Ignored,
    }
}

/// 常见错误码给一句人话解释
fn tencent_hint(code: i64) -> &'static str {
    match code {
        4002 => "（鉴权失败，请检查 AppID / SecretID / SecretKey 与系统时间）",
        4003 => "（该 AppID 未开通语音识别服务）",
        4004 => "（资源包已耗尽）",
        4005 => "（账户欠费）",
        4006 => "（并发超限）",
        4007 => "（音频格式与参数不一致）",
        4008 => "（超过 15 秒未发送音频）",
        6001 => "（境外网络调用，请关闭代理）",
        _ => "",
    }
}

/// 语言 → 腾讯实时识别引擎映射。
///
/// 腾讯的实时引擎按语言分型号（16k_zh / 16k_yue / 16k_en / 16k_ja / 16k_ko）；
/// 法语/德语/俄语/西班牙语只有录音文件识别支持、没有实时引擎，
/// 明确报错（让主人换服务商或改回「自动」），绝不悄悄用错引擎瞎识别。
fn engine_for_language(lang: &str) -> Result<Option<&'static str>> {
    match lang {
        // 跟随用户在「语音服务」页自选的引擎
        "auto" => Ok(None),
        "zh" => Ok(Some("16k_zh")),
        "yue" => Ok(Some("16k_yue")),
        "en" => Ok(Some("16k_en")),
        "ja" => Ok(Some("16k_ja")),
        "ko" => Ok(Some("16k_ko")),
        // 这四种语言腾讯只有录音文件识别、没有实时引擎：明确报错
        // （让主人换服务商或改回「自动」），绝不悄悄用中文引擎瞎识别
        "fr" | "de" | "ru" | "es" => bail!("腾讯云暂不支持该语言的实时识别，请换服务商或改回「自动」"),
        // 配置文件手改坏了等极端情况：当「自动」处理，别拦着主人用
        _ => Ok(None),
    }
}

/// 生成带签名的 WebSocket 地址（`smooth` 控制 filter_modal，对应「口语顺滑」开关；
/// `language` 非 auto 时覆盖为对应语言的引擎）
pub fn signed_url(cfg: &TencentConfig, smooth: bool, language: &str) -> Result<String> {
    signed_url_at(cfg, now_secs(), smooth, language)
}

fn signed_url_at(cfg: &TencentConfig, now: u64, smooth: bool, language: &str) -> Result<String> {
    let app_id = cfg.app_id.trim();
    let secret_id = cfg.secret_id.trim();
    let secret_key = cfg.secret_key.trim();
    if app_id.is_empty() || secret_id.is_empty() || secret_key.is_empty() {
        bail!("腾讯云需要填写 App ID、Secret ID、Secret Key");
    }
    // 选了识别语言（非 auto）就用对应语言引擎，覆盖用户自选的引擎；
    // auto 才用用户在「语音服务」页选的引擎
    let engine = match engine_for_language(language)? {
        Some(e) => e,
        None => {
            let e = cfg.engine_model_type.trim();
            if e.is_empty() {
                bail!("腾讯云需要选择引擎模型");
            }
            e
        }
    };

    let voice_id = uuid::Uuid::new_v4().to_string();
    // BTreeMap 天然按 key 字典序排列，签名原文要求字典序
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("secretid".into(), secret_id.to_string());
    params.insert("timestamp".into(), now.to_string());
    params.insert("expired".into(), (now + 3600).to_string());
    params.insert("nonce".into(), nonce().to_string());
    params.insert("engine_model_type".into(), engine.to_string());
    params.insert("voice_id".into(), voice_id);
    params.insert("voice_format".into(), "1".into()); // pcm
    params.insert("needvad".into(), "1".into());
    params.insert("convert_num_mode".into(), "1".into());
    params.insert("filter_dirty".into(), "0".into());
    // 语气词过滤（=「口语顺滑」开关）：之前写死 0，开关对腾讯是摆设
    params.insert("filter_modal".into(), if smooth { "1" } else { "0" }.into());
    params.insert("filter_punc".into(), "0".into());

    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let origin = format!("asr.cloud.tencent.com/asr/v2/{app_id}?{query}");

    let mut mac = Hmac::<Sha1>::new_from_slice(secret_key.as_bytes()).context("SecretKey 非法")?;
    mac.update(origin.as_bytes());
    let signature = STANDARD.encode(mac.finalize().into_bytes());
    let encoded = utf8_percent_encode(&signature, NON_ALPHANUMERIC).to_string();

    Ok(format!("wss://{origin}&signature={encoded}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 随机正整数（nonce，腾讯要求最长 10 位）。
///
/// 别写成 `% 9_999_999_999` 再 `as u32`：u32 最大只有 4_294_967_295，
/// 100 亿取模的结果会被截断，得到的根本不是注释里说的那个区间。
fn nonce() -> u32 {
    1 + (uuid::Uuid::new_v4().as_u128() % 999_999_999) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TencentConfig {
        TencentConfig {
            app_id: "1259223000".into(),
            secret_id: "AKIDexample".into(),
            secret_key: "SecretKeyExample".into(),
            engine_model_type: "Hy-ASR-3.0-preview".into(),
        }
    }

    /// 回归（断线被说成"没有识别到内容"）：腾讯收到**任何**关闭帧都当成
    /// "识别正常结束"，关闭码和原因全丢 —— 包括 60 秒引擎上限踢人、鉴权掉线。
    /// 主人得到的却是「没有识别到内容，请靠近麦克风再说一次」，完全被带偏。
    #[test]
    fn abnormal_close_reports_the_reason() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;

        let mut t = Tencent::new();
        let close = Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: "engine limit".into(),
        }));
        match t.parse(close) {
            Parsed::Error(e) => {
                assert!(e.contains("策略拒绝"), "应给出关闭码含义：{e}");
                assert!(e.contains("engine limit"), "应带上服务端原因：{e}");
            }
            other => panic!("异常关闭要报错，实际：{other:?}"),
        }

        // 正常收尾仍按正常结束
        let mut normal = Tencent::new();
        let frame = Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        }));
        assert!(matches!(normal.parse(frame), Parsed::Finished));
        assert!(matches!(Tencent::new().parse(Message::Close(None)), Parsed::Finished));
    }

    #[test]
    fn hmac_sha1_matches_known_vector() {
        // RFC 2202 标准测试向量，验证 HMAC-SHA1 + Base64 管线正确
        let mut mac = Hmac::<Sha1>::new_from_slice(b"key").unwrap();
        mac.update(b"The quick brown fox jumps over the lazy dog");
        let digest = mac.finalize().into_bytes();
        assert_eq!(hex(&digest), "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9");
    }

    #[test]
    fn url_contains_required_params_and_signature() {
        let url = signed_url_at(&cfg(), 1_743_000_000, true, "auto").unwrap();
        assert!(url.starts_with("wss://asr.cloud.tencent.com/asr/v2/1259223000?"));
        for key in [
            "engine_model_type=Hy-ASR-3.0-preview",
            "expired=1743003600",
            "nonce=",
            "secretid=AKIDexample",
            "timestamp=1743000000",
            "voice_format=1",
            "signature=",
        ] {
            assert!(url.contains(key), "缺少参数 {key}");
        }
        // 参数必须字典序排列
        let query = url
            .split('?')
            .nth(1)
            .unwrap()
            .split("&signature=")
            .next()
            .unwrap();
        let keys: Vec<&str> = query
            .split('&')
            .filter_map(|kv| kv.split('=').next())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    /// 「口语顺滑」开关必须真的落到 filter_modal 参数里
    #[test]
    fn smooth_toggle_reaches_filter_modal() {
        let on = signed_url_at(&cfg(), 1_743_000_000, true, "auto").unwrap();
        assert!(on.contains("filter_modal=1"));
        let off = signed_url_at(&cfg(), 1_743_000_000, false, "auto").unwrap();
        assert!(off.contains("filter_modal=0"));
    }

    /// 识别语言 → 腾讯引擎映射：非 auto 时覆盖用户自选引擎；
    /// auto 时用用户自选引擎；不支持的语言（腾讯无实时引擎）明确报错。
    #[test]
    fn language_overrides_engine_and_unsupported_errors() {
        // 选了英语 → 引擎必须换成 16k_en（而不是用户自选的 Hy-ASR-3.0-preview）
        let en = signed_url_at(&cfg(), 1_743_000_000, true, "en").unwrap();
        assert!(en.contains("engine_model_type=16k_en"), "选英语应切 16k_en：{en}");
        let yue = signed_url_at(&cfg(), 1_743_000_000, true, "yue").unwrap();
        assert!(yue.contains("engine_model_type=16k_yue"));
        // auto → 用户自选引擎
        let auto = signed_url_at(&cfg(), 1_743_000_000, true, "auto").unwrap();
        assert!(auto.contains("engine_model_type=Hy-ASR-3.0-preview"));
        // 法语：腾讯没有实时引擎，必须报错而不是拿中文引擎瞎识别
        let fr = signed_url_at(&cfg(), 1_743_000_000, true, "fr");
        assert!(fr.is_err(), "无实时引擎的语言必须报错");
        assert!(fr.unwrap_err().to_string().contains("暂不支持"));
    }

    #[test]
    fn missing_credentials_is_rejected() {
        let mut c = cfg();
        c.secret_key = "  ".into();
        assert!(signed_url_at(&c, 0, true, "auto").is_err());
    }

    /// 回归（腾讯最后一包永不结束会话 → 主人松手后白等 8 秒超时）：
    /// 腾讯把结束标记 `final=1` 和最后一句的文本放在**同一个包**里，而解析是
    /// 先判 slice_type 的 —— 于是这一包被当成普通文本，`done` 永远不置位。
    #[test]
    fn final_packet_with_text_also_ends_the_session() {
        match parse_json(
            r#"{"code":0,"result":{"slice_type":2,"voice_text_str":"你好，世界。"},"final":1}"#,
        ) {
            Parsed::Text { text, finished, .. } => {
                assert_eq!(text, "你好，世界。");
                assert!(finished, "带文本的最后一包也要结束会话，否则只能白等到超时");
            }
            other => panic!("应解析出稳态结果，实际：{other:?}"),
        }
    }

    #[test]
    fn parses_slice_types() {
        match parse_json(
            r#"{"code":0,"result":{"slice_type":1,"voice_text_str":"你好"},"final":0}"#,
        ) {
            Parsed::Text { kind, text, .. } => {
                assert_eq!(kind, Kind::Partial);
                assert_eq!(text, "你好");
            }
            _ => panic!("应解析出中间结果"),
        }
        match parse_json(
            r#"{"code":0,"result":{"slice_type":2,"voice_text_str":"你好，世界。"},"final":1}"#,
        ) {
            Parsed::Text { kind, text, .. } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "你好，世界。");
            }
            _ => panic!("应解析出稳态结果"),
        }
        // final=1 但没有文本 → 结束
        assert!(matches!(
            parse_json(r#"{"code":0,"result":{"slice_type":0,"voice_text_str":""},"final":1}"#),
            Parsed::Finished
        ));
    }

    /// `slice_type` 的合法值只有 0/1/2；字段缺失必须按 0（"不是稳态/终态"）
    /// 处理，落到与 0 相同的分支，绝不能因为"字段没来"就当成别的东西。
    ///
    /// 0 的情况后来加上了文本分支（首帧就带文本，见下面两条用例），所以这里的
    /// 期望随之变成"按 0 分支处理"：非终包是中间结果、终包是定稿。它守的仍然是
    /// "缺字段落到 0 分支"，而不是被当成某个别的值。
    #[test]
    fn missing_slice_type_stays_in_the_value_domain() {
        // 缺 slice_type、有文本、非终包 → 按 0 → 中间结果
        match parse_json(r#"{"code":0,"result":{"voice_text_str":"你好"},"final":0}"#) {
            Parsed::Text {
                kind,
                text,
                finished,
                ..
            } => {
                assert_eq!(kind, Kind::Partial);
                assert_eq!(text, "你好");
                assert!(!finished);
            }
            other => panic!("缺 slice_type 应落到 0 分支（中间结果），实际：{other:?}"),
        }
        // 缺 slice_type、有文本、是终包 → 按 0 分支 → 定稿并结束
        match parse_json(r#"{"code":0,"result":{"voice_text_str":"你好"},"final":1}"#) {
            Parsed::Text {
                kind,
                text,
                finished,
                ..
            } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "你好");
                assert!(finished);
            }
            other => panic!("缺 slice_type 应落到 0 分支（定稿），实际：{other:?}"),
        }
    }

    /// 回归（"没有识别到内容"）：`slice_type == 0`（首帧）且带文本时，
    /// 非终包必须按中间结果上报，不能被无条件丢掉。
    #[test]
    fn first_slice_with_text_is_reported_as_partial() {
        match parse_json(
            r#"{"code":0,"result":{"slice_type":0,"voice_text_str":"你好"},"final":0}"#,
        ) {
            Parsed::Text {
                kind,
                text,
                finished,
                ..
            } => {
                assert_eq!(kind, Kind::Partial);
                assert_eq!(text, "你好");
                assert!(!finished);
            }
            other => panic!("首帧带文本应按中间结果上报，实际：{other:?}"),
        }
    }

    /// 回归（"没有识别到内容"）：`slice_type == 0` 且这一包就是终包时，
    /// 必须按定稿（`finished: true`）上报 —— 否则整段文本连同结束标记一起消失。
    #[test]
    fn final_first_slice_with_text_is_reported_as_final() {
        match parse_json(
            r#"{"code":0,"result":{"slice_type":0,"voice_text_str":"你好"},"final":1}"#,
        ) {
            Parsed::Text {
                kind,
                text,
                finished,
                ..
            } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "你好");
                assert!(finished, "终包标记必须一起上报，否则会话白等 8 秒");
            }
            other => panic!("终态首帧带文本应按定稿上报，实际：{other:?}"),
        }
    }

    #[test]
    fn auth_error_is_readable() {
        match parse_json(r#"{"code":4002,"message":"auth failed"}"#) {
            Parsed::Error(e) => assert!(e.contains("鉴权失败") && e.contains("auth failed")),
            _ => panic!("应报错"),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
