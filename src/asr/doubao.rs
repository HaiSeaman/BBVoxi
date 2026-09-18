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
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

const MSG_FULL_CLIENT_REQUEST: u8 = 0b0001;
const MSG_AUDIO_ONLY: u8 = 0b0010;
const MSG_ERROR_RESPONSE: u8 = 0b1111;
/// flags：帧头后带 4 字节序号
const FLAG_WITH_SEQUENCE: u8 = 0b0001;
/// flags：最后一包
const FLAG_LAST_PACKET: u8 = 0b0010;
/// flags：帧头后带 4 字节事件号
const FLAG_WITH_EVENT: u8 = 0b0100;
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
            Message::Close(frame) => {
                self.finished = true;
                // 正常收尾是 Close(Normal, "finish last sequence")；
                // 异常关闭（过载、鉴权掉线）不能糊成"没有识别到内容"，要把原因说清楚
                match frame {
                    Some(f) if f.code != CloseCode::Normal => {
                        let reason = if f.reason.is_empty() {
                            "无附加信息"
                        } else {
                            f.reason.as_str()
                        };
                        return Parsed::Error(format!(
                            "豆包提前关闭连接（{}）：{reason}",
                            close_hint(f.code)
                        ));
                    }
                    _ => return Parsed::Finished,
                }
            }
            _ => return Parsed::Ignored,
        };
        let Some(frame) = decode(&bytes) else {
            return Parsed::Ignored;
        };
        // v3 的响应正文里根本没有 code 字段（顶层就是 audio_info / result），
        // 错误码只在「错误帧」的帧头里。之前用 unwrap_or(-1) 兜底，
        // 于是每一条正常的响应都被当成 code=-1 的错误 —— 一开录就报错。
        // 仍保留「正文里有显式 code」这条分支：资源 ID 可在设置里改，
        // 换成 1.0 资源（volc.bigasr.sauc.*）时响应结构未必一致。
        let json_code = frame.payload["code"].as_i64().unwrap_or(0);
        if frame.msg_type == MSG_ERROR_RESPONSE || frame.code != 0 || json_code != 0 {
            self.finished = true;
            let code = if frame.code != 0 {
                frame.code
            } else {
                json_code as i32
            };
            return Parsed::Error(describe(code, &frame.payload));
        }

        let result = &frame.payload["result"];
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

        // 结束标记在帧头的 flags 里（服务端实测最后一包 flags=0b0011），JSON 里没有该字段
        if frame.flags & FLAG_LAST_PACKET != 0 {
            self.finished = true;
        }
        // `finished` 必须跟着文本一起上报：最后一包可能同时带着新定稿的句子，
        // 只回文本的话会话就等不到结束（主人松手后白等 8 秒超时）。
        if !newly_final.is_empty() {
            Parsed::Text {
                kind: Kind::Final,
                text: newly_final,
                finished: self.finished,
            }
        } else if !partial.is_empty() {
            Parsed::Text {
                kind: Kind::Partial,
                text: partial,
                finished: self.finished,
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

/// 解出来的一帧服务端响应
struct Frame {
    msg_type: u8,
    flags: u8,
    /// 错误码，只有错误帧（0b1111）才在帧头里带；正常帧为 0
    code: i32,
    /// 正文（JSON；无序列化时为纯文本字符串）
    payload: serde_json::Value,
}

/// 按官方 v3 协议拆帧：header(4B) + [序号 4B] + [事件 4B] + [错误码 4B] + 长度 4B + 正文。
/// 返回 None 表示帧不完整或无法解析。
fn decode(bytes: &[u8]) -> Option<Frame> {
    if bytes.len() < 4 {
        return None;
    }
    let header_size = (bytes[0] & 0x0f) as usize * 4;
    let msg_type = bytes[1] >> 4;
    let flags = bytes[1] & 0x0f;
    let serialization = bytes[2] >> 4;
    let compression = bytes[2] & 0x0f;
    let mut offset = header_size;
    if flags & FLAG_WITH_SEQUENCE != 0 {
        offset += 4;
    }
    if flags & FLAG_WITH_EVENT != 0 {
        offset += 4;
    }
    // 错误帧的帧头里先放错误码，再放长度
    let mut code = 0i32;
    if msg_type == MSG_ERROR_RESPONSE {
        code = i32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?);
        offset += 4;
    }
    let size =
        u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
    offset += 4;
    let payload = bytes.get(offset..offset + size)?;
    let data = if compression == COMPRESSION_GZIP {
        let mut out = Vec::new();
        GzDecoder::new(payload).read_to_end(&mut out).ok()?;
        out
    } else {
        payload.to_vec()
    };
    let value = if serialization == SERIALIZATION_JSON {
        serde_json::from_slice(&data).ok()?
    } else {
        serde_json::Value::String(String::from_utf8_lossy(&data).into_owned())
    };
    Some(Frame {
        msg_type,
        flags,
        code,
        payload: value,
    })
}

/// 错误响应里尽量挖出可读信息
fn describe(code: i32, value: &serde_json::Value) -> String {
    let body = ["message", "error", "err_msg", "payload_msg"]
        .iter()
        .find_map(|k| value[k].as_str())
        .map(str::to_string)
        .or_else(|| value.as_str().map(str::to_string))
        .unwrap_or_else(|| value.to_string());
    match error_hint(code) {
        Some(hint) => format!("豆包返回错误（code={code}，{hint}）：{body}"),
        None => format!("豆包返回错误（code={code}）：{body}"),
    }
}

/// WebSocket 关闭码的中文解释
fn close_hint(code: CloseCode) -> &'static str {
    match code {
        CloseCode::Away => "服务端临时离开",
        CloseCode::Policy => "策略拒绝（多为鉴权或额度问题）",
        CloseCode::Size => "消息过大",
        CloseCode::Protocol => "协议错误",
        CloseCode::Unsupported => "不支持的数据类型",
        CloseCode::Abnormal => "连接异常中断",
        CloseCode::Invalid => "数据无效",
        CloseCode::Extension => "扩展协商失败",
        CloseCode::Error => "服务端内部错误",
        CloseCode::Restart => "服务端重启",
        CloseCode::Again => "服务端要求重试",
        _ => "连接被关闭",
    }
}

/// 官方错误码的中文解释，便于用户自查
fn error_hint(code: i32) -> Option<&'static str> {
    match code {
        4_500_000_1 => Some("请求参数无效"),
        4_500_000_2 => Some("空音频"),
        4_500_008_1 => Some("等包超时"),
        4_500_015_1 => Some("音频格式不正确"),
        5_500_003_1 => Some("服务器繁忙"),
        _ if (5_500_0000..5_600_0000).contains(&code) => Some("服务内部错误"),
        _ => None,
    }
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

    /// 回归（结束标记与文本同包 → `done` 不置位，松手后白等 8 秒）：
    /// 服务端实测最后一包 flags=0b0011，它可能**同时**带着新定稿的文本。
    /// 解析必须把"这一包是最后一包"这个信息一起交给上层，不能被文本吞掉。
    #[test]
    fn last_packet_with_text_also_finishes() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = json!({
            "result": { "utterances": [ { "text": "最后一句", "definite": true } ] }
        });
        match d.parse(real_frame(0b0011, Some(1), &body.to_string(), false)) {
            Parsed::Text { text, finished, .. } => {
                assert_eq!(text, "最后一句");
                assert!(finished, "带着文本的最后一包也要结束会话");
            }
            other => panic!("应返回文本，实际：{other:?}"),
        }
    }

    #[test]
    fn parses_streaming_and_definite_utterances() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        // 真实响应顶层就是 audio_info / result，没有 code、也没有 payload_msg 包装
        let body = json!({
            "audio_info": { "duration": 2000 },
            "result": {
                "text": "你好世界",
                "utterances": [
                    { "text": "你好", "definite": true },
                    { "text": "世界", "definite": false }
                ]
            }
        });
        let parsed = d.parse(server_frame(&body.to_string()));
        match parsed {
            Parsed::Text { kind, text, .. } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "你好");
            }
            _ => panic!("应返回稳态结果"),
        }
        // 下一包：第二句也变为稳态，且标记为最后一包
        let body2 = json!({
            "audio_info": { "duration": 3696 },
            "result": { "utterances": [
                { "text": "你好", "definite": true },
                { "text": "世界", "definite": true }
            ] }
        });
        match d.parse(real_frame(0b0011, Some(1), &body2.to_string(), false)) {
            Parsed::Text { kind, text, .. } => {
                assert_eq!(kind, Kind::Final);
                assert_eq!(text, "世界"); // 只发新增的，不重复
            }
            _ => panic!("应返回新增稳态结果"),
        }
        assert!(d.finished);
    }

    /// 回归：服务端在建连后立刻下发的这条「只有 log_id」的帧，
    /// 曾因为正文里没有 code 字段被误判成 code=-1 的错误，一开录就失败。
    #[test]
    fn handshake_frame_without_code_is_not_an_error() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = r#"{"result":{"additions":{"log_id":"20260911023351F39AC493AB93B01E9883"}}}"#;
        match d.parse(real_frame(0b0000, None, body, false)) {
            Parsed::Ignored => {}
            other => panic!("只有 log_id 的空结果应被忽略，实际：{other:?}"),
        }
        assert!(!d.finished);
    }

    /// 回归：最后一包靠帧头 flags 的 0b0010 位识别，正文里没有 is_last_package。
    #[test]
    fn last_package_flag_marks_finished() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = r#"{"audio_info":{"duration":2100},"result":{"additions":{"log_id":"x"}}}"#;
        match d.parse(real_frame(0b0011, Some(1), body, false)) {
            Parsed::Finished => {}
            other => panic!("最后一包应结束会话，实际：{other:?}"),
        }
        assert!(d.finished);
    }

    #[test]
    fn non_zero_code_is_an_error() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let body = json!({ "code": 45000001, "message": "invalid resource id" });
        match d.parse(server_frame(&body.to_string())) {
            Parsed::Error(e) => {
                assert!(e.contains("invalid resource id"));
                assert!(e.contains("请求参数无效"));
            }
            _ => panic!("应报错"),
        }
    }

    /// 错误帧的错误码在帧头里（正文之前），不在 JSON 中
    #[test]
    fn error_frame_code_comes_from_header() {
        let mut d = Doubao::new(&cfg(), &Options::default());
        let f = error_frame(45000002, r#"{"message":"empty audio"}"#);
        match d.parse(f) {
            Parsed::Error(e) => {
                assert!(e.contains("45000002"), "应带上错误码：{e}");
                assert!(e.contains("空音频"), "应给出中文解释：{e}");
                assert!(e.contains("empty audio"), "应带上原始信息：{e}");
            }
            _ => panic!("应报错"),
        }
        assert!(d.finished);
    }

    /// 服务端异常关闭不能糊成"没有识别到内容"，要把原因说出来
    #[test]
    fn abnormal_close_reports_the_reason() {
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;

        let mut d = Doubao::new(&cfg(), &Options::default());
        let close = Message::Close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: "server busy".into(),
        }));
        match d.parse(close) {
            Parsed::Error(e) => {
                assert!(e.contains("服务端内部错误"), "应给出关闭码含义：{e}");
                assert!(e.contains("server busy"), "应带上服务端原因：{e}");
            }
            other => panic!("异常关闭应报错，实际：{other:?}"),
        }

        // 正常收尾：豆包会发 Close(Normal, "finish last sequence")
        let mut d2 = Doubao::new(&cfg(), &Options::default());
        assert!(matches!(d2.parse(Message::Close(None)), Parsed::Finished));
    }

    /// 排障用：真实连接豆包，打印服务端原始帧结构与 parse 的判定结果。
    /// 二进制 crate 没有别的办法走线协议，协议一变只能靠它抓真帧。
    ///
    /// 必需：BBVOXI_DOUBAO_KEY
    /// 可选：BBVOXI_DOUBAO_URL（默认 bigmodel_async）、BBVOXI_DOUBAO_NOSTREAM=1（高精度端点）、
    ///       BBVOXI_DOUBAO_WAV（16k/16bit/mono 裸 PCM，不发语音时发静音）
    /// 跑法：cargo test live_probe -- --ignored --nocapture
    #[ignore]
    #[tokio::test]
    async fn live_probe_dump_frames() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        use tokio_tungstenite::connect_async;

        let key = std::env::var("BBVOXI_DOUBAO_KEY").expect("需要 BBVOXI_DOUBAO_KEY");
        let url = std::env::var("BBVOXI_DOUBAO_URL")
            .unwrap_or_else(|_| "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async".into());
        let res = std::env::var("BBVOXI_DOUBAO_RES")
            .unwrap_or_else(|_| "volc.seedasr.sauc.duration".into());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut req = url.as_str().into_client_request().unwrap();
        req.headers_mut()
            .insert("X-Api-Key", HeaderValue::from_str(&key).unwrap());
        req.headers_mut()
            .insert("X-Api-Resource-Id", HeaderValue::from_str(&res).unwrap());
        req.headers_mut().insert(
            "X-Api-Connect-Id",
            HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
        );
        let (ws, resp) = connect_async(req).await.expect("连接失败");
        println!("=== 握手响应状态：{}", resp.status());
        for (k, v) in resp.headers() {
            println!("    {k}: {}", v.to_str().unwrap_or("?"));
        }

        use futures_util::{SinkExt, StreamExt};
        let (mut sink, mut stream) = ws.split();

        // 全量客户端请求（BBVOXI_DOUBAO_NOSTREAM=1 时走高精度 nostream 端点）
        let mut c = cfg();
        if std::env::var("BBVOXI_DOUBAO_NOSTREAM").map(|v| v == "1").unwrap_or(false) {
            c.high_accuracy = true;
        }
        let d = Doubao::new(&c, &Options::default());
        let start = d.start_messages();
        println!("=== 发送启动帧 {} 字节：{:02x?}", start[0].clone().into_data().len(), &start[0].clone().into_data()[..8]);
        sink.send(start[0].clone()).await.unwrap();
        let mut parser = Doubao::new(&cfg(), &Options::default());

        // 默认 20 帧 × 100ms 静音；设置 BBVOXI_DOUBAO_WAV 时改发真实语音（16k/16bit/mono 裸 PCM）
        let audio: Vec<u8> = match std::env::var("BBVOXI_DOUBAO_WAV") {
            Ok(p) => std::fs::read(&p).expect("读取 PCM 失败"),
            Err(_) => pcm_bytes(&vec![0i16; 1600 * 20]),
        };
        let chunks: Vec<Vec<u8>> = audio.chunks(3200).map(|c| c.to_vec()).collect();
        let total = chunks.len();
        println!("=== 共 {total} 个音频分片（100ms/片）");
        for (i, bytes) in chunks.iter().enumerate() {
            let last = i + 1 == total;
            sink.send(audio_frame(bytes, last)).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // 边发边收
            while let Ok(Some(Ok(msg))) =
                tokio::time::timeout(std::time::Duration::from_millis(1), stream.next()).await
            {
                dump(&msg, &mut parser);
            }
        }

        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(std::time::Duration::from_secs(6), stream.next()).await
        {
            dump(&msg, &mut parser);
        }
        println!("=== 探测结束");
    }

    /// 刻意不复用 `decode`：坏帧也要能打印出来，用它解析就什么都不剩了。
    fn dump(msg: &Message, parser: &mut Doubao) {
        let parsed = format!("{:?}", parser.parse(msg.clone()));
        let Message::Binary(bytes) = msg else {
            println!("[非二进制帧] {msg:?}\n     解析结果：{parsed}");
            return;
        };
        if bytes.len() < 4 {
            println!("[过短] {bytes:02x?}");
            return;
        }
        let mt = bytes[1] >> 4;
        let flags = bytes[1] & 0x0f;
        let ser = bytes[2] >> 4;
        let comp = bytes[2] & 0x0f;
        let mut off = 4usize;
        let mut seq = None;
        if flags & 0x01 != 0 {
            seq = Some(i32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()));
            off += 4;
        }
        if flags & 0x04 != 0 {
            off += 4;
        }
        let mut code = None;
        if mt == 0b1111 {
            code = Some(u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()));
            off += 4;
        }
        let size = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let payload = &bytes[off..std::cmp::min(off + size, bytes.len())];
        let text = if comp == 1 {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut GzDecoder::new(payload), &mut out)
                .map(|_| String::from_utf8_lossy(&out).to_string())
                .unwrap_or_else(|e| format!("<解压失败 {e}>"))
        } else {
            String::from_utf8_lossy(payload).to_string()
        };
        println!(
            "[帧] type={mt:#06b} flags={flags:#06b} ser={ser} comp={comp} seq={seq:?} code={code:?} size={size}\n     正文：{}",
            &text[..std::cmp::min(text.len(), 800)]
        );
        println!("     解析结果：{parsed}");
    }

    /// 按真实协议拼一条服务端 full server response（0b1001）
    fn real_frame(flags: u8, seq: Option<i32>, json_body: &str, gz: bool) -> Message {
        let payload: Vec<u8> = if gz {
            gzip(json_body.as_bytes())
        } else {
            json_body.as_bytes().to_vec()
        };
        let comp = if gz { COMPRESSION_GZIP } else { 0 };
        let header = [
            0x11u8,
            (0b1001 << 4) | flags,
            (SERIALIZATION_JSON << 4) | comp,
            0x00,
        ];
        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        if let Some(s) = seq {
            buf.extend_from_slice(&s.to_be_bytes());
        }
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        Message::binary(buf)
    }

    /// 默认响应帧：带序号 + gzip
    fn server_frame(json_body: &str) -> Message {
        real_frame(FLAG_WITH_SEQUENCE, Some(7), json_body, true)
    }

    /// 错误帧（0b1111）：帧头后紧跟 4 字节错误码，再是长度和正文
    fn error_frame(code: i32, json_body: &str) -> Message {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x11u8, 0b1111_0000, SERIALIZATION_JSON << 4, 0x00]);
        buf.extend_from_slice(&code.to_be_bytes());
        buf.extend_from_slice(&(json_body.len() as u32).to_be_bytes());
        buf.extend_from_slice(json_body.as_bytes());
        Message::binary(buf)
    }
}
