//! 千问（阿里云百炼）实时语音识别：run-task → 二进制音频流 → result-generated → finish-task。
//! 地址已核实：wss://dashscope.aliyuncs.com/api-ws/v1/inference（鉴权走 Authorization: Bearer）。

use super::{pcm_bytes, Kind, Parsed};
use crate::config::QwenConfig;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

pub struct Qwen {
    task_id: String,
    model: String,
    /// 语义标点（跟随「自动添加标点」开关）：true 由 LLM 加标点更准但首字更慢，
    /// false 用 VAD 断句、出字更快
    semantic_punctuation: bool,
    pub ready: bool,
}

impl Qwen {
    pub fn new(cfg: &QwenConfig, auto_punctuation: bool) -> Self {
        Self {
            task_id: uuid::Uuid::new_v4().to_string(),
            model: cfg.model.trim().to_string(),
            semantic_punctuation: auto_punctuation,
            ready: false,
        }
    }

    pub fn start_messages(&self) -> Vec<Message> {
        let body = json!({
            "header": { "action": "run-task", "task_id": self.task_id, "streaming": "duplex" },
            "payload": {
                "task_group": "audio",
                "task": "asr",
                "function": "recognition",
                "model": self.model,
                "parameters": {
                    "format": "pcm",
                    "sample_rate": 16_000,
                    "semantic_punctuation_enabled": self.semantic_punctuation,
                    // 静音时保持连接，否则长时间不说话会被服务端断开
                    "heartbeat": true
                },
                "input": {}
            }
        });
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

    /// 「自动添加标点」开关必须真的落到请求参数里（之前不传，开关是摆设）
    #[test]
    fn punctuation_toggle_reaches_request() {
        let cfg = QwenConfig::default();
        let on = match &Qwen::new(&cfg, true).start_messages()[0] {
            Message::Text(t) => t.to_string(),
            _ => panic!("应为文本帧"),
        };
        assert!(on.contains(r#""semantic_punctuation_enabled":true"#));
        let off = match &Qwen::new(&cfg, false).start_messages()[0] {
            Message::Text(t) => t.to_string(),
            _ => panic!("应为文本帧"),
        };
        assert!(off.contains(r#""semantic_punctuation_enabled":false"#));
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
