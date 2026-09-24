//! ASR 客户端：三家统一抽象。
//! 连接后由一个后台任务持续读取 WebSocket 帧转发到 channel，会话任务只做「发音频 + 收事件」两件事。

pub mod doubao;
pub mod qwen;
pub mod tencent;

use anyhow::{bail, Context, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::{Config, Provider};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// rustls 0.23 必须在使用前安装进程级加密后端，否则第一次 TLS 握手会 panic：
/// "Could not automatically determine the process-level CryptoProvider"。
/// 之前正是这个 panic 让「测试麦克风与连接」一点就闪退。
fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 已经被别处安装过会返回 Err，忽略即可
        let _ = rustls::crypto::ring::default_provider().install_default();
        crate::log::log("TLS 加密后端已就绪（ring）");
    });
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 中间结果，用于设置窗的实时预览（不打字）
    Partial,
    /// 稳态结果，需要拼接进最终文本
    Final,
}

/// 一次 provider 帧的解析结果
#[derive(Debug)]
pub enum Parsed {
    /// 带文本的结果。
    ///
    /// `finished` = 这一包同时是会话的**最后一包**：腾讯（`final=1`）和豆包
    /// （最后一包 flags）都会把结束标记和最后一句的文本塞进同一个包，
    /// 所以"结束"必须能跟正文一起上报，否则就会被正文吞掉 —— 会话只能靠
    /// 8 秒超时收场。
    Text {
        kind: Kind,
        text: String,
        finished: bool,
    },
    Finished,
    Error(String),
    Ignored,
}

pub enum Backend {
    Qwen(qwen::Qwen),
    Doubao(doubao::Doubao),
    Tencent(tencent::Tencent),
}

impl Backend {
    fn ready(&self) -> bool {
        match self {
            Backend::Qwen(q) => q.ready,
            Backend::Doubao(_) | Backend::Tencent(_) => true,
        }
    }

    fn start_messages(&self) -> Vec<Message> {
        match self {
            Backend::Qwen(q) => q.start_messages(),
            Backend::Doubao(d) => d.start_messages(),
            Backend::Tencent(_) => Vec::new(),
        }
    }

    fn audio_message(&mut self, pcm: &[i16]) -> Vec<Message> {
        match self {
            Backend::Qwen(q) => q.audio_messages(pcm),
            Backend::Doubao(d) => d.audio_messages(pcm),
            Backend::Tencent(t) => t.audio_messages(pcm),
        }
    }

    fn finish_messages(&mut self) -> Vec<Message> {
        match self {
            Backend::Qwen(q) => q.finish_messages(),
            Backend::Doubao(d) => d.finish_messages(),
            Backend::Tencent(t) => t.finish_messages(),
        }
    }

    fn parse(&mut self, msg: Message) -> Parsed {
        match self {
            Backend::Qwen(q) => q.parse(msg),
            Backend::Doubao(d) => d.parse(msg),
            Backend::Tencent(t) => t.parse(msg),
        }
    }
}

pub struct AsrClient {
    sink: SplitSink<Ws, Message>,
    msgs: UnboundedReceiver<Message>,
    reader: tokio::task::JoinHandle<()>,
    backend: Backend,
    /// 识别结果的累积状态（定稿文本 / 中间结果 / 是否结束 / 服务端错误）
    pub state: Transcript,
}

/// 识别结果的累积状态。
///
/// 单独抽出来是为了能**脱离 WebSocket 单测**：「最后一包要不要结束会话」这类
/// 逻辑以前长在 `AsrClient::handle` 里，没有真连接就测不到 —— 而它恰好是
/// 「松手后白等 8 秒」那个 bug 的所在地。
#[derive(Default)]
pub struct Transcript {
    /// 已定稿的文本
    pub final_text: String,
    /// 当前这句还没定稿的中间结果
    pub partial_text: String,
    /// 服务端已明确结束（或出错）
    pub done: bool,
    /// 服务端返回的错误（需要在界面上提示，不能只写日志）
    pub last_error: Option<String>,
}

impl Transcript {
    /// 吸收一条解析结果；返回界面要显示的完整文本（有文本可显示时）
    pub fn absorb(&mut self, parsed: Parsed) -> Option<(Kind, String)> {
        match parsed {
            Parsed::Text {
                kind,
                text,
                finished,
            } => {
                match kind {
                    Kind::Partial => self.partial_text = text,
                    Kind::Final => {
                        self.final_text.push_str(&text);
                        self.partial_text.clear();
                    }
                }
                // 先收正文再结束：顺序反了最后一句就丢了
                if finished {
                    self.done = true;
                }
                Some((kind, self.display_text()))
            }
            Parsed::Finished => {
                self.done = true;
                None
            }
            Parsed::Error(e) => {
                self.done = true;
                crate::log::log(format!("ASR 返回错误：{e}"));
                self.last_error = Some(e);
                None
            }
            Parsed::Ignored => None,
        }
    }

    /// 实时显示的完整文本（已定稿部分 + 当前中间结果）
    pub fn display_text(&self) -> String {
        if self.partial_text.is_empty() {
            self.final_text.clone()
        } else {
            format!("{}{}", self.final_text, self.partial_text)
        }
    }
}

impl AsrClient {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        ensure_crypto_provider();
        let (url, headers) = request_spec(cfg)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("非法接口地址：{url}"))?;
        for (name, value) in headers {
            let name = name
                .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
                .with_context(|| format!("非法请求头名：{name}"))?;
            request
                .headers_mut()
                .insert(name, HeaderValue::from_str(&value).context("非法请求头值")?);
        }

        let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
            .await
            .context("连接 ASR 服务超时（5 秒）")?
            .context("连接 ASR 服务失败（请检查网络、API Key 与接口地址）")?;

        let (sink, stream) = ws.split();
        let (tx, msgs) = unbounded_channel();
        let reader = tokio::spawn(forward_frames(stream, tx));

        let mut client = AsrClient {
            sink,
            msgs,
            reader,
            backend: build_backend(cfg),
            state: Transcript::default(),
        };

        for msg in client.backend.start_messages() {
            client.sink.send(msg).await.context("发送启动指令失败")?;
        }

        // 千问需要等 task-started 才能发音频；豆包/腾讯立即可发
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        while !client.backend.ready() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("服务端握手超时");
            }
            match tokio::time::timeout(remaining, client.msgs.recv()).await {
                Ok(Some(msg)) => match client.backend.parse(msg) {
                    Parsed::Error(e) => bail!(e),
                    Parsed::Finished => bail!("服务端在握手阶段结束了任务"),
                    _ => {}
                },
                Ok(None) => bail!("连接被服务端关闭"),
                Err(_) => bail!("服务端握手超时"),
            }
        }
        crate::log::log(format!("ASR 已连接：{}", cfg.provider.label()));
        Ok(client)
    }

    pub async fn send_audio(&mut self, pcm: &[i16]) -> Result<()> {
        for msg in self.backend.audio_message(pcm) {
            self.sink.send(msg).await.context("发送音频失败")?;
        }
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<()> {
        for msg in self.backend.finish_messages() {
            self.sink.send(msg).await.context("发送结束指令失败")?;
        }
        Ok(())
    }

    /// 取一条消息（None 表示连接已结束）
    pub async fn recv(&mut self) -> Option<Message> {
        self.msgs.recv().await
    }

    pub fn handle(&mut self, msg: Message) -> Option<(Kind, String)> {
        let parsed = self.backend.parse(msg);
        self.state.absorb(parsed)
    }

    /// 最终交付文本：已定稿部分 + 还没定稿的尾部。
    ///
    /// 必须把未定稿的尾部也算进去：实时输入已经把这段字打进了目标程序，
    /// 若这里只返回已定稿部分，收尾对账时反而会把用户看到的字删掉。
    pub fn take_result(&self) -> String {
        merge_result(&self.state.final_text, &self.state.partial_text)
    }
}

/// 合并已定稿文本与未定稿尾部（去掉首尾空白）
pub fn merge_result(final_text: &str, partial_text: &str) -> String {
    if partial_text.is_empty() {
        final_text.trim().to_string()
    } else if final_text.is_empty() {
        partial_text.trim().to_string()
    } else {
        format!("{}{}", final_text, partial_text).trim().to_string()
    }
}

impl Drop for AsrClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn forward_frames(
    mut stream: SplitStream<Ws>,
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
) {
    while let Some(item) = stream.next().await {
        match item {
            Ok(msg) => {
                let closing = matches!(msg, Message::Close(_));
                if tx.send(msg).is_err() || closing {
                    break;
                }
            }
            Err(e) => {
                crate::log::log(format!("WebSocket 读取结束：{e}"));
                break;
            }
        }
    }
}

fn build_backend(cfg: &Config) -> Backend {
    match cfg.provider {
        Provider::Qwen => Backend::Qwen(qwen::Qwen::new(&cfg.qwen, cfg.options.auto_punctuation)),
        Provider::Doubao => Backend::Doubao(doubao::Doubao::new(&cfg.doubao, &cfg.options)),
        Provider::Tencent => Backend::Tencent(tencent::Tencent::new()),
    }
}

/// 各家接口地址与鉴权请求头（地址写死在 config.rs）
pub fn request_spec(cfg: &Config) -> Result<(String, Vec<(String, String)>)> {
    match cfg.provider {
        Provider::Qwen => Ok((
            cfg.qwen.base_url.clone(),
            vec![(
                "Authorization".to_string(),
                format!("Bearer {}", cfg.qwen.api_key.trim()),
            )],
        )),
        Provider::Doubao => Ok((
            cfg.doubao.endpoint().to_string(),
            vec![
                (
                    "X-Api-Key".to_string(),
                    cfg.doubao.api_key.trim().to_string(),
                ),
                (
                    "X-Api-Resource-Id".to_string(),
                    cfg.doubao.resource_id.trim().to_string(),
                ),
                (
                    "X-Api-Connect-Id".to_string(),
                    uuid::Uuid::new_v4().to_string(),
                ),
            ],
        )),
        Provider::Tencent => Ok((
            tencent::signed_url(&cfg.tencent, cfg.options.smooth)?,
            Vec::new(),
        )),
    }
}

/// i16 PCM → 小端字节流
pub fn pcm_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：走一遍真实的 TLS 连接路径（故意用无效 Key）。
    /// 期望得到「明确的错误」，而不是 panic —— 之前 rustls 缺少加密后端时
    /// 会在这里 panic，那时的 release 配置是 `panic = "abort"`，表现为点测试
    /// 按钮直接闪退（现在已改为 `unwind` + panic hook，见 `Cargo.toml`）。
    #[tokio::test]
    async fn connect_with_invalid_key_returns_error_instead_of_crashing() {
        let mut cfg = Config::default();
        cfg.qwen.api_key = "sk-invalid-for-test".into();
        let result = AsrClient::connect(&cfg).await;
        assert!(result.is_err(), "无效 Key 必须返回错误而不是 panic / 成功");
        let msg = format!("{:#}", result.err().unwrap());
        assert!(!msg.is_empty());
    }

    /// 收尾文本不能丢掉「还没定稿的尾部」：实时输入已经把它打进目标程序了，
    /// 少了它，收尾对账会把用户看到的字删掉。
    #[test]
    fn merge_result_keeps_unfinalized_tail() {
        assert_eq!(merge_result("已经说完。", ""), "已经说完。");
        assert_eq!(merge_result("", "还在这句"), "还在这句");
        assert_eq!(merge_result("第一句。", "还在这句"), "第一句。还在这句");
        assert_eq!(merge_result("  有空白  ", ""), "有空白");
    }

    /// 回归（结束标记与文本同包时 `done` 不置位 → 松手后白等 8 秒超时）：
    /// 腾讯和豆包都会把「最后一包」和「最后一句的文本」放进同一个包里。
    /// 解析层已经把这件事说清楚了，累积这一层不能把它吞掉。
    #[test]
    fn transcript_marks_done_when_a_text_packet_also_finishes() {
        let mut t = Transcript::default();
        let shown = t.absorb(Parsed::Text {
            kind: Kind::Final,
            text: "你好".into(),
            finished: true,
        });
        assert_eq!(shown.unwrap().1, "你好", "文本不能被结束标记吞掉");
        assert!(t.done, "带结束标记的文本包必须结束会话，否则只能白等到超时");
    }

    /// 普通文本包不能提前结束会话（否则后面的句子全丢）
    #[test]
    fn transcript_keeps_going_without_the_finish_flag() {
        let mut t = Transcript::default();
        t.absorb(Parsed::Text {
            kind: Kind::Final,
            text: "你好".into(),
            finished: false,
        });
        assert!(!t.done);
        assert_eq!(t.display_text(), "你好");
    }

    /// 服务端报错也必须结束会话，并把原因留给界面
    #[test]
    fn transcript_records_server_errors() {
        let mut t = Transcript::default();
        assert!(t.absorb(Parsed::Error("鉴权失败".into())).is_none());
        assert!(t.done);
        assert_eq!(t.last_error.as_deref(), Some("鉴权失败"));
    }
}
