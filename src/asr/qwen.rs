//! 千问（阿里云百炼）实时语音识别：run-task → 二进制音频流 → result-generated → finish-task。
//! 地址已核实：wss://dashscope.aliyuncs.com/api-ws/v1/inference（鉴权走 Authorization: Bearer）。

use super::{hotword_list, pcm_bytes, Kind, Parsed};
use crate::config::{Options, QwenConfig};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

pub struct Qwen {
    task_id: String,
    model: String,
    options: Options,
    pub ready: bool,
}

impl Qwen {
    pub fn new(cfg: &QwenConfig, options: &Options) -> Self {
        Self {
            task_id: uuid::Uuid::new_v4().to_string(),
            model: cfg.model.trim().to_string(),
            options: options.clone(),
            ready: false,
        }
    }

    pub fn start_messages(&self) -> Vec<Message> {
        let mut body = json!({
            "header": { "action": "run-task", "task_id": self.task_id, "streaming": "duplex" },
            "payload": {
                "task_group": "audio",
                "task": "asr",
                "function": "recognition",
                "model": self.model,
                "parameters": {
                    "format": "pcm",
                    "sample_rate": 16_000,
                    // 语义标点（跟随「自动添加标点」开关）：true 由 LLM 加标点更准但首字更慢，
                    // false 用 VAD 断句、出字更快
                    "semantic_punctuation_enabled": self.options.auto_punctuation,
                    // 去语气词（跟随「口语顺滑」开关）：之前从未传过，开关对千问是摆设
                    "disfluency_removal_enabled": self.options.smooth,
                    // 中间结果：官方默认关，显式打开才能「边说边出字」
                    "intermediate_result_enabled": true,
                    // 近场听写 VAD。官方默认 far_field_meeting_16k 是「远场会议」场景
                    // （会议室麦克风收全场声音），对麦克风说话的输入法场景用近场更准
                    "vad_model": "near_meeting_16k",
                    // 静音时保持连接，否则长时间不说话会被服务端断开
                    "heartbeat": true
                },
                "input": {}
            }
        });
        // 个人词典（热词）：即时热词词表 + 上下文增强，两条都上，
        // 专有名词（人名/品牌/术语）的命中率才明显。
        let hotwords = hotword_list(&self.options.hotwords);
        if !hotwords.is_empty() {
            // 词 → 权重（官方范围 1~5，4 = 高优先但不过分抢占正常识别）
            let vocab: serde_json::Map<String, serde_json::Value> = hotwords
                .iter()
                .map(|w| (w.clone(), json!(4)))
                .collect();
            body["payload"]["parameters"]["vocabulary"] = json!(vocab);
            body["payload"]["input"]["context"] = json!([{
                "role": "user",
                "content": [{ "type": "input_text", "text": hotwords.join(" ") }]
            }]);
        }
        vec![Message::text(body.to_string())]
    }

    pub fn audio_messages(&mut self, pcm: &[i16]) -> Vec<Message> {
        vec![Message::binary(pcm_bytes(pcm))]
    }

    pub fn finish_messages(&mut self) -> Vec<Message> {
        let body = json!({
            "header": { "action": "finish-task", "task_id": self.task_id, "streaming": "duplex" },
            "payload": { "input": {} }
        });
        vec![Message::text(body.to_string())]
    }

    pub fn parse(&mut self, msg: Message) -> Parsed {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => return Parsed::Finished,
            _ => return Parsed::Ignored,
        };
        parse_json(&text, &mut self.ready)
    }
}

fn parse_json(text: &str, ready: &mut bool) -> Parsed {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Parsed::Ignored;
    };
    match v["header"]["event"].as_str().unwrap_or("") {
        "task-started" => {
            *ready = true;
            Parsed::Ignored
        }
        "task-finished" => Parsed::Finished,
        "task-failed" => {
            let code = v["header"]["error_code"].as_str().unwrap_or("TASK_FAILED");
            let msg = v["header"]["error_message"].as_str().unwrap_or("");
            Parsed::Error(format!("{code}: {msg}"))
        }
        "result-generated" => {
            let sentence = &v["payload"]["output"]["sentence"];
            if sentence["heartbeat"].as_bool().unwrap_or(false) {
                return Parsed::Ignored; // 心跳包不计入
            }
            let text = sentence["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Parsed::Ignored;
            }
            let kind = if sentence["sentence_end"].as_bool().unwrap_or(false) {
                Kind::Final
            } else {
                Kind::Partial
            };
            // 千问的结束是单独一条 task-finished，不会跟文本同包。
            // `sentence_id` 用来对"同一句被重复上报"做幂等（见 `Transcript`）：
            // 没有它，服务端把某句的定稿多发一次，主人屏幕上就是同一句两遍。
            Parsed::Text {
                kind,
                text,
                finished: false,
                id: sentence["sentence_id"].as_i64(),
            }
        }
        _ => Parsed::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_json(cfg: &QwenConfig, options: &Options) -> serde_json::Value {
        match &Qwen::new(cfg, options).start_messages()[0] {
            Message::Text(t) => serde_json::from_str(t).expect("启动帧应为合法 JSON"),
            _ => panic!("应为文本帧"),
        }
    }

    fn options_with(f: impl FnOnce(&mut Options)) -> Options {
        let mut o = Options::default();
        f(&mut o);
        o
    }

    /// 「自动添加标点」开关必须真的落到请求参数里（之前不传，开关是摆设）
    #[test]
    fn punctuation_toggle_reaches_request() {
        let cfg = QwenConfig::default();
        let on = start_json(&cfg, &options_with(|o| o.auto_punctuation = true));
        assert_eq!(on["payload"]["parameters"]["semantic_punctuation_enabled"], true);
        let off = start_json(&cfg, &options_with(|o| o.auto_punctuation = false));
        assert_eq!(
            off["payload"]["parameters"]["semantic_punctuation_enabled"],
            false
        );
    }

    /// 回归（千问 3.1）：「口语顺滑」开关必须落到 disfluency_removal_enabled。
    /// 之前千问的启动帧从不带这个参数，开关对千问是摆设、界面还显示"暂不支持"。
    #[test]
    fn smooth_toggle_reaches_disfluency_removal() {
        let cfg = QwenConfig::default();
        let on = start_json(&cfg, &options_with(|o| o.smooth = true));
        assert_eq!(on["payload"]["parameters"]["disfluency_removal_enabled"], true);
        let off = start_json(&cfg, &options_with(|o| o.smooth = false));
        assert_eq!(
            off["payload"]["parameters"]["disfluency_removal_enabled"],
            false
        );
    }

    /// 千问 3.1 固定参数：近场 VAD + 显式开启中间结果。
    /// 官方默认远场（far_field_meeting_16k）与中间结果默认关，
    /// 两项不显式传，输入法场景出字又慢又少。
    #[test]
    fn near_field_vad_and_intermediate_results() {
        let body = start_json(&QwenConfig::default(), &Options::default());
        assert_eq!(body["payload"]["parameters"]["vad_model"], "near_meeting_16k");
        assert_eq!(
            body["payload"]["parameters"]["intermediate_result_enabled"],
            true
        );
    }

    /// 个人词典（热词）：千问走「即时热词词表 + 上下文增强」两条通道，
    /// 没填词时一个字段都不能多（行为与升级前完全一致）。
    #[test]
    fn hotwords_reach_vocabulary_and_context() {
        let cfg = QwenConfig::default();
        // 没填词：不带 vocabulary / context
        let empty = start_json(&cfg, &Options::default());
        assert!(empty["payload"]["parameters"]["vocabulary"].is_null());
        assert!(empty["payload"]["input"]["context"].is_null());

        let o = options_with(|o| {
            o.hotwords = "  宝可梦 \n\n张三丰\n  \nBBVoxi\n".into();
        });
        let body = start_json(&cfg, &o);
        // 去空行、去首尾空格后恰好 3 个词
        assert_eq!(body["payload"]["parameters"]["vocabulary"]["宝可梦"], 4);
        assert_eq!(body["payload"]["parameters"]["vocabulary"]["张三丰"], 4);
        assert_eq!(body["payload"]["parameters"]["vocabulary"]["BBVoxi"], 4);
        assert_eq!(body["payload"]["parameters"]["vocabulary"].as_object().map(|m| m.len()), Some(3));
        assert_eq!(
            body["payload"]["input"]["context"][0]["content"][0]["text"],
            "宝可梦 张三丰 BBVoxi"
        );
        assert_eq!(body["payload"]["input"]["context"][0]["role"], "user");
    }

    fn feed(raw: &str) -> Parsed {
        let mut ready = false;
        parse_json(raw, &mut ready)
    }

    #[test]
    fn parses_task_started_and_finished() {
        let mut ready = false;
        assert!(matches!(
            parse_json(
                r#"{"header":{"event":"task-started"},"payload":{}}"#,
                &mut ready
            ),
            Parsed::Ignored
        ));
        assert!(
            ready,
            "task-started 必须把 ready 置起来（否则不会开始送音频）"
        );

        let finished = parse_json(
            r#"{"header":{"event":"task-finished"},"payload":{}}"#,
            &mut ready,
        );
        assert!(
            matches!(finished, Parsed::Finished),
            "task-finished 必须以 Finished 结束会话，实际 {finished:?}"
        );
    }

    #[test]
    fn partial_and_final_sentences() {
        let partial = feed(
            r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":"今天天气","sentence_end":false,"sentence_id":1}},"usage":null}}"#,
        );
        match partial {
            Parsed::Text {
                kind,
                text,
                finished,
                ..
            } => {
                assert_eq!(kind, Kind::Partial);
                assert_eq!(text, "今天天气");
                assert!(!finished, "千问普通中间结果不该结束会话");
            }
            _ => panic!("应解析出中间结果"),
        }

        let fin = feed(
            r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":"今天天气不错。","sentence_end":true,"sentence_id":1}},"usage":{"duration":3}}}"#,
        );
        match fin {
            Parsed::Text { kind, text, id, .. } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "今天天气不错。");
                assert_eq!(
                    id,
                    Some(1),
                    "必须把 sentence_id 带出来：没有它，同一句被重复上报时就会多打一份字"
                );
            }
            _ => panic!("应解析出最终结果"),
        }
    }

    #[test]
    fn heartbeat_is_ignored() {
        assert!(matches!(
            feed(
                r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":"啊","heartbeat":true,"sentence_id":0}}}}"#
            ),
            Parsed::Ignored
        ));
    }

    #[test]
    fn task_failed_carries_server_message() {
        match feed(
            r#"{"header":{"event":"task-failed","error_code":"CLIENT_ERROR","error_message":"request timeout after 23 seconds."}}"#,
        ) {
            Parsed::Error(e) => assert!(e.contains("request timeout")),
            _ => panic!("应报错"),
        }
    }
}
