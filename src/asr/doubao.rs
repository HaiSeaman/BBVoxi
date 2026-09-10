//! 豆包（火山引擎）大模型流式语音识别。
//! 地址已核实：双向流式 wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async
//!             高精度   wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream
//! 二进制协议：4 字节头 + 4 字节 payload 长度(大端) + payload；JSON 体 gzip 压缩。
//! 2.0 资源（volc.seedasr.sauc.duration）的 StreamMode 只能是 1 或 2。

use super::{pcm_bytes, Kind, Parsed};
use crate::config::{DoubaoConfig, Options};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;
use std::io::{Read, Write};
use tokio_tungstenite::tungstenite::Message;

const MSG_FULL_CLIENT_REQUEST: u8 = 0b0001;
const MSG_AUDIO_ONLY: u8 = 0b0010;
const MSG_ERROR_RESPONSE: u8 = 0b1111;
const FLAG_LAST_PACKET: u8 = 0b0010;
const SERIALIZATION_JSON: u8 = 0b0001;
const COMPRESSION_GZIP: u8 = 0b0001;

pub struct Doubao {
    stream_mode: u8,
    options: Options,
    /// 留一帧缓冲，最后一帧要打「结束包」标记
    buffered: Option<Vec<u8>>,
    /// 已经计入最终文本的 utterances 数量
    consumed: usize,
    finished: bool,
}

impl Doubao {
    pub fn new(cfg: &DoubaoConfig, options: &Options) -> Self {
        Self {
            // 2.0 资源只接受 1（整句）或 2（双向流式 + 二次识别）；高精度模式用 nostream 端点配 1
            stream_mode: if cfg.high_accuracy { 1 } else { 2 },
            options: options.clone(),
            buffered: None,
            consumed: 0,
            finished: false,
        }
    }

    pub fn start_messages(&self) -> Vec<Message> {
        let body = json!({
            "audio": { "format": "pcm", "codec": "raw", "rate": 16_000, "bits": 16, "channel": 1 },
            "request": {
                "model_name": "bigmodel",
                "StreamMode": self.stream_mode,
                "enable_itn": true,
                "enable_punc": self.options.auto_punctuation,
                "enable_ddc": self.options.smooth,
                "enable_nonstream": self.stream_mode == 2,
                "show_utterances": true,
                "result_type": "full",
                "end_window_size": 800
            }
        });
        vec![frame(
            MSG_FULL_CLIENT_REQUEST,
            0,
            SERIALIZATION_JSON,
            COMPRESSION_GZIP,
            &gzip(body.to_string().as_bytes()),
        )]
    }

    pub fn audio_messages(&mut self, pcm: &[i16]) -> Vec<Message> {
        let bytes = pcm_bytes(pcm);
        let mut out = Vec::new();
        if let Some(prev) = self.buffered.replace(bytes) {
            out.push(audio_frame(&prev, false));
        }
        out
    }

    pub fn finish_messages(&mut self) -> Vec<Message> {
        let last = self.buffered.take().unwrap_or_default();
        vec![audio_frame(&last, true)]
    }

    pub fn parse(&mut self, msg: Message) -> Parsed {
        let bytes = match msg {
            Message::Binary(b) => b.to_vec(),
            Message::Close(_) => {
                self.finished = true;
                return Parsed::Finished;
            }
            _ => return Parsed::Ignored,
        };
        let Some((msg_type, parsed)) = decode(&bytes) else {
            return Parsed::Ignored;
        };
        if msg_type == MSG_ERROR_RESPONSE {
            self.finished = true;
            return Parsed::Error(describe(&parsed));
        }
        if parsed["code"].as_i64().unwrap_or(-1) != 0 {
            self.finished = true;
            return Parsed::Error(describe(&parsed));
        }

        let result = &parsed["payload_msg"]["result"];
        let mut newly_final = String::new();
        let mut partial = String::new();
        if let Some(list) = result["utterances"].as_array() {
            while self.consumed < list.len() {
                let u = &list[self.consumed];
                if !u["definite"].as_bool().unwrap_or(false) {
                    break;
                }
                let text = u["text"].as_str().unwrap_or("").trim();
                if !text.is_empty() {
                    newly_final.push_str(text);
                }
                self.consumed += 1;
            }
            if let Some(last) = list.last() {
                if !last["definite"].as_bool().unwrap_or(false) {
                    partial = last["text"].as_str().unwrap_or("").trim().to_string();
                }
            }
        } else if let Some(text) = result["text"].as_str() {
            // 未开 show_utterances 时退回整段文本
            partial = text.trim().to_string();
        }

        if parsed["is_last_package"].as_bool().unwrap_or(false) {
            self.finished = true;
        }
        if !newly_final.is_empty() {
            Parsed::Text {
                kind: Kind::Final,
                text: newly_final,
            }
        } else if !partial.is_empty() {
            Parsed::Text {
                kind: Kind::Partial,
                text: partial,
            }
        } else if self.finished {
            Parsed::Finished
        } else {
            Parsed::Ignored
        }
    }
}

/// 组装一个协议帧
fn frame(msg_type: u8, flags: u8, serialization: u8, compression: u8, payload: &[u8]) -> Message {
    let header = [
        0b0001_0001, // 协议版本 1 + 头长度 1（4 字节）
        (msg_type << 4) | flags,
        (serialization << 4) | compression,
        0x00,
    ];
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    Message::binary(buf)
}

fn audio_frame(pcm: &[u8], last: bool) -> Message {
    let flags = if last { FLAG_LAST_PACKET } else { 0 };
    frame(MSG_AUDIO_ONLY, flags, 0, 0, pcm)
}

/// 解出 (消息类型, payload JSON)。返回 None 表示帧不完整或不是 JSON。
fn decode(bytes: &[u8]) -> Option<(u8, serde_json::Value)> {
    if bytes.len() < 8 {
        return None;
    }
    let msg_type = bytes[1] >> 4;
    let flags = bytes[1] & 0x0f;
    let compression = bytes[2] & 0x0f;
    let mut offset = 4;
    if flags & 0b0001 != 0 {
        offset += 4; // 带序号
    }
    if bytes.len() < offset + 4 {
        return None;
    }
    let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
    offset += 4;
    if bytes.len() < offset + size {
        return None;
    }
    let payload = &bytes[offset..offset + size];
    let data = if compression == COMPRESSION_GZIP {
        let mut out = Vec::new();
        GzDecoder::new(payload).read_to_end(&mut out).ok()?;
        out
    } else {
        payload.to_vec()
    };
    let value = serde_json::from_slice(&data).ok()?;
    Some((msg_type, value))
}

/// 错误响应里尽量挖出可读信息
fn describe(value: &serde_json::Value) -> String {
    for key in ["message", "error", "payload_msg"] {
        if let Some(s) = value[key].as_str() {
            return format!("豆包返回错误：{s}");
        }
    }
    format!(
        "豆包返回错误（code={}）：{}",
        value["code"].as_i64().unwrap_or(-1),
        value
    )
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DoubaoConfig {
        DoubaoConfig::default()
    }

    #[test]
    fn full_client_request_header_matches_protocol() {
        let d = Doubao::new(&cfg(), &Options::default());
        let msgs = d.start_messages();
        let Message::Binary(bytes) = &msgs[0] else {
            panic!("应为二进制帧")
        };
        assert_eq!(&bytes[0..4], &[0x11, 0x10, 0x11, 0x00]);
        let size = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(size, bytes.len() - 8);
        // payload 为 gzip
        let mut out = Vec::new();
        GzDecoder::new(&bytes[8..])
            .read_to_end(&mut out)
            .expect("payload 应能解压");
        let body: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(body["audio"]["rate"], 16_000);
        assert_eq!(body["request"]["StreamMode"], 2);
        assert_eq!(body["request"]["model_name"], "bigmodel");
    }

    #[test]
    fn high_accuracy_mode_uses_stream_mode_1() {
        let mut c = cfg();
        c.high_accuracy = true;
        let d = Doubao::new(&c, &Options::default());
        let Message::Binary(bytes) = &d.start_messages()[0] else {
            panic!()
        };
        let mut out = Vec::new();
        GzDecoder::new(&bytes[8..]).read_to_end(&mut out).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(body["request"]["StreamMode"], 1);
        assert_eq!(body["request"]["enable_nonstream"], false);
    }

    #[test]
    fn audio_frames_buffer_one_and_last_packet_is_flagged() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        // 第一帧被缓冲，不立即发送
        assert!(d.audio_messages(&[1, 2, 3]).is_empty());
        // 第二帧到达时发出第一帧（非结束包）
        let first = d.audio_messages(&[4, 5, 6]);
        assert_eq!(first.len(), 1);
        let Message::Binary(bytes) = &first[0] else {
            panic!()
        };
        assert_eq!(bytes[1], 0x20);
        assert_eq!(&bytes[8..], &[1, 0, 2, 0, 3, 0]);
        // 结束包带 LAST_PACKET 标记
        let last = d.finish_messages();
        let Message::Binary(bytes) = &last[0] else {
            panic!()
        };
        assert_eq!(bytes[1], 0x22);
        assert_eq!(&bytes[8..], &[4, 0, 5, 0, 6, 0]);
    }

    #[test]
    fn parses_streaming_and_definite_utterances() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = json!({
            "code": 0,
            "is_last_package": false,
            "payload_msg": { "result": {
                "text": "你好世界",
                "utterances": [
                    { "text": "你好", "definite": true },
                    { "text": "世界", "definite": false }
                ]
            } }
        });
        let parsed = d.parse(server_frame(&body.to_string()));
        match parsed {
            Parsed::Text { kind, text } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "你好");
            }
            _ => panic!("应返回稳态结果"),
        }
        // 下一包：第二句也变为稳态
        let body2 = json!({
            "code": 0,
            "is_last_package": true,
            "payload_msg": { "result": { "utterances": [
                { "text": "你好", "definite": true },
                { "text": "世界", "definite": true }
            ] } }
        });
        match d.parse(server_frame(&body2.to_string())) {
            Parsed::Text { kind, text } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "世界"); // 只发新增的，不重复
            }
            _ => panic!("应返回新增稳态结果"),
        }
        assert!(d.finished);
    }

    #[test]
    fn non_zero_code_is_an_error() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = json!({ "code": 45000001, "message": "invalid resource id" });
        match d.parse(server_frame(&body.to_string())) {
            Parsed::Error(e) => assert!(e.contains("invalid resource id")),
            _ => panic!("应报错"),
        }
    }

    /// 构造一个服务端响应帧（JSON + gzip + 序号）
    fn server_frame(json_body: &str) -> Message {
        let payload = gzip(json_body.as_bytes());
        let header = [0x11u8, 0x91, 0x11, 0x00];
        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&7u32.to_be_bytes()); // sequence
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        Message::binary(buf)
    }
}
