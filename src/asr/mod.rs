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
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::{Config, Provider};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 单次 WebSocket 发送的超时上限。
///
/// 为什么必须要有它：Wi-Fi 掉线（没有 RST）或服务端只读不读时，TCP 写缓冲会被
/// 填满，此时 `sink.send(...).await` **永远不会返回** —— 会话主循环就彻底卡死在
/// 这里，主人松开快捷键也收不到，之后所有热键都失灵。加超时后最坏 5 秒就报错收场。
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// 服务端帧通道的容量。用有界通道：服务端帧若无上限地堆进内存，
/// 一旦上层一时处理不过来就会把内存吃光。
const MSG_CHANNEL_CAPACITY: usize = 64;

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
    ///
    /// `id` = 这句在服务端那边的编号（千问的 `sentence_id`）。有了它才能对
    /// **重复上报**做幂等：同一句被报两次时，追加就会变成"打多一份字"。
    /// 腾讯的协议没有编号，这里恒为 `None`；豆包也没有服务端编号，但它会给
    /// "整段定稿快照"用一个固定编号（见 `doubao.rs`），靠"同号替换"实现二遍
    /// 识别改写的幂等。
    Text {
        kind: Kind,
        text: String,
        finished: bool,
        id: Option<i64>,
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

    /// 启动指令。返回 Result：豆包的启动帧要 gzip 压缩，压缩失败必须向上报错，
    /// 绝不能发出"标志写着已压缩、内容却是空的"那种自相矛盾的帧。
    fn start_messages(&self) -> Result<Vec<Message>> {
        match self {
            Backend::Qwen(q) => Ok(q.start_messages()),
            Backend::Doubao(d) => d.start_messages(),
            Backend::Tencent(_) => Ok(Vec::new()),
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
    msgs: Receiver<Message>,
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
    /// 最近一次拼进 `final_text` 的句子编号，以及那次拼进去的**字节数**。
    ///
    /// 为什么必须要它：服务端对同一个句子**可能重复上报定稿**，而
    /// `final_text.push_str` 是无条件追加 —— 追加两次，主人屏幕上就是同一句
    /// 重复一遍（正是"说话时会多打一份字"这个现象）。
    /// 有编号就能做幂等：同号 → 把上次那段替换掉；更旧的号 → 丢。
    ///
    /// 千问用的是服务端的 `sentence_id`；豆包没有服务端编号，但它会给"整段
    /// 定稿快照"一个固定编号来触发同样的替换语义（见 `doubao.rs`）。腾讯恒定
    /// 为 None，行为与以前一模一样。
    ///
    /// **这两项必须成对**：`last_final_bytes` 永远是"最后追加的那一段"的长度，
    /// 而 `last_final_id` 是那一段的编号（那一段没带编号就是 None）。对不上，
    /// 替换时就会从错误的字节位置截断 —— 截错地方比重复一遍更糟。
    last_final_id: Option<i64>,
    last_final_bytes: usize,
}

impl Transcript {
    /// 吸收一条解析结果；返回界面要显示的完整文本（有文本可显示时）
    pub fn absorb(&mut self, parsed: Parsed) -> Option<(Kind, String)> {
        match parsed {
            Parsed::Text {
                kind,
                text,
                finished,
                id,
            } => {
                let changed = self.apply_text(kind, &text, id);
                // 先收正文再结束：顺序反了最后一句就丢了
                if finished {
                    self.done = true;
                }
                // 内容没变（重复上报被挡住）就不回报，界面才不会跟着抖一下
                changed.then(|| (kind, self.display_text()))
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

    /// 把一条文本并进累积状态，返回内容是否真的变了（重复上报返回 false）。
    ///
    /// 幂等只对**带编号**的结果生效；没有编号时保持原来的"无条件追加"，
    /// 免得改坏腾讯的行为（腾讯的协议没有编号）。
    fn apply_text(&mut self, kind: Kind, text: &str, id: Option<i64>) -> bool {
        match kind {
            Kind::Partial => {
                // 已经定稿的句子又冒出一条"中间结果"，那是迟到的旧包。
                // 收下它，屏幕上就会在刚定稿的那句后面再重复一遍 —— 典型的
                // "说到一半突然多出一份字"。丢掉即可：这句话已经定稿过了。
                if let (Some(id), Some(last)) = (id, self.last_final_id) {
                    if id <= last {
                        crate::log::log(format!(
                            "丢弃迟到的中间结果（句子 #{id} 已定稿，最新 #{last}）：{text}"
                        ));
                        return false;
                    }
                }
                if self.partial_text == text {
                    return false;
                }
                self.partial_text = text.to_string();
                true
            }
            Kind::Final => {
                let mut replaced = false;
                if let (Some(id), Some(last)) = (id, self.last_final_id) {
                    if id == last {
                        // 同一个编号再报一次定稿：可能是"补上标点的修订版"，
                        // 也可能是豆包二遍识别改写过的整段快照。
                        // 把上次那段换掉重写，而不是在后面再追加一段 ——
                        // 一替换一追加的差别，就是屏幕上会不会出现两遍同一句。
                        replaced = true;
                        let keep = self.final_text.len().saturating_sub(self.last_final_bytes);
                        self.final_text.truncate(keep);
                    } else if id < last {
                        // 比最近定稿更旧的句子又报了一次：它早就拼进去了，
                        // 再来一条只会在屏幕上重复一遍（正是"多打一份字"）。
                        // 宁可丢掉"对旧句的修正"，也不让同一句出现两次。
                        crate::log::log(format!(
                            "丢弃重复上报的旧定稿（句子 #{id}，最新 #{last}）：{text}"
                        ));
                        return false;
                    }
                }
                self.final_text.push_str(text);
                // 编号和长度必须成对更新，否则下一次"同号替换"会从错误的字节位置截断
                self.last_final_bytes = text.len();
                self.last_final_id = id;
                if let Some(id) = id {
                    // 一条定稿只记一条日志（替换/首次各一种），免得排障时被刷屏
                    if replaced {
                        crate::log::log(format!(
                            "同号定稿再次上报（句子 #{id}），已替换为最新内容：{text}"
                        ));
                    } else {
                        crate::log::log(format!("定稿（句子 #{id}）：{text}"));
                    }
                }
                self.partial_text.clear();
                true
            }
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
        // 有界通道：服务端帧不能无上限地堆进内存（见 MSG_CHANNEL_CAPACITY）
        let (tx, msgs) = channel(MSG_CHANNEL_CAPACITY);
        let reader = tokio::spawn(forward_frames(stream, tx));

        let mut client = AsrClient {
            sink,
            msgs,
            reader,
            backend: build_backend(cfg),
            state: Transcript::default(),
        };

        for msg in client.backend.start_messages()? {
            client.send_timed(msg, "发送启动指令").await?;
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
            self.send_timed(msg, "发送音频").await?;
        }
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<()> {
        for msg in self.backend.finish_messages() {
            self.send_timed(msg, "发送结束指令").await?;
        }
        Ok(())
    }

    /// 发一帧，并强制带上超时。
    ///
    /// 为什么必须加超时：见 `SEND_TIMEOUT` 的说明 —— 网络半死时 `sink.send`
    /// 的 await 会永远不返回，整个会话就此卡死。这里统一包一层。
    async fn send_timed(&mut self, msg: Message, what: &str) -> Result<()> {
        tokio::time::timeout(SEND_TIMEOUT, self.sink.send(msg))
            .await
            .with_context(|| format!("发送超时（网络可能已断开）：{what}"))?
            .with_context(|| format!("{what}失败"))?;
        Ok(())
    }

    /// 主动发一条关闭帧并 flush，让服务端看到的是「正常关闭」而不是异常断开。
    ///
    /// 为什么要有它：以前全程不发关闭帧，`Drop` 又直接 `reader.abort()`，服务端
    /// 只能把连接当成异常中断，关闭原因/错误码的语义全部丢失，排障时看不到线索。
    /// 这是给调用方可选的优雅收尾入口 —— `Drop` 里不能 await，所以不做在里面；
    /// 调用方若想优雅收尾可显式调用它（不调也不影响原有行为）。
    #[allow(dead_code)] // 供调用方按需使用；当前会话流程未强制调用它
    pub async fn close(&mut self) -> Result<()> {
        self.send_timed(Message::Close(None), "发送关闭帧").await?;
        // `send` 已隐含一次 flush，这里再显式刷一次，确保关闭帧真的落到线上
        tokio::time::timeout(SEND_TIMEOUT, self.sink.flush())
            .await
            .with_context(|| "刷新关闭帧超时（网络可能已断开）")?
            .context("刷新关闭帧失败")?;
        Ok(())
    }

    /// 取一条消息（None 表示连接已结束）
    pub async fn recv(&mut self) -> Option<Message> {
        let msg = self.msgs.recv().await;
        // 收到对端的关闭帧时，必须先把「关闭应答」flush 出去再把它交出去 ——
        // 否则我们这条连接对服务端而言仍是异常断开，关闭诊断信息一样拿不到。
        // 这里能用 `&mut self`（`recv` 是 async 的），而 `Drop` 里不能 await。
        if matches!(msg, Some(Message::Close(_))) {
            let _ = tokio::time::timeout(SEND_TIMEOUT, async {
                let _ = self.sink.send(Message::Close(None)).await;
                let _ = self.sink.flush().await;
            })
            .await;
        }
        msg
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

async fn forward_frames(mut stream: SplitStream<Ws>, tx: Sender<Message>) {
    // 通道满时丢帧的计数（见下）：绝不能因为通道满就去 await，否则读取任务被
    // 卡住，TCP 接收缓冲又会被填满、最终拖垮整条连接。
    let mut dropped: u64 = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(msg) => {
                let closing = matches!(msg, Message::Close(_));
                // 用 try_send（不阻塞）：满了就丢这一帧，读取任务永远向前走
                if let Err(e) = tx.try_send(msg) {
                    match e {
                        TrySendError::Full(_) => {
                            dropped += 1;
                            // 限流记日志：第一条 + 之后每 64 条记一次，
                            // 既能看到"在丢帧"，又不会每帧刷一条把日志冲爆
                            if dropped == 1 || dropped % 64 == 0 {
                                crate::log::log(format!(
                                    "ASR 帧处理不过来，已丢弃 {dropped} 帧（通道容量 {MSG_CHANNEL_CAPACITY}）"
                                ));
                            }
                        }
                        TrySendError::Closed(_) => break,
                    }
                }
                if closing {
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
            id: None,
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
            id: None,
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

    fn text(kind: Kind, s: &str, id: Option<i64>) -> Parsed {
        Parsed::Text {
            kind,
            text: s.into(),
            finished: false,
            id,
        }
    }

    /// 回归（"说话时会多打一份字"）：服务端把**同一句**的定稿又报了一次。
    /// 无条件 `push_str` 会把它接在后面，主人屏幕上就是同一句出现两遍。
    /// 有 `sentence_id` 时必须替换上一次那段，而不是再追加一段。
    #[test]
    fn repeated_final_for_the_same_sentence_replaces_instead_of_appending() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Final, "今天天气不错。", Some(1)));
        assert_eq!(t.final_text, "今天天气不错。");

        t.absorb(text(Kind::Final, "今天天气不错。", Some(1)));
        assert_eq!(
            t.final_text, "今天天气不错。",
            "同一句的定稿被重复上报时不能又追加一遍（这就是「多打一份字」）"
        );

        // 内容被修正时同样只留最新的一份
        t.absorb(text(Kind::Final, "今天天气不错呀。", Some(1)));
        assert_eq!(t.final_text, "今天天气不错呀。");
    }

    /// 回归（同上）：句子**已经定稿**之后，又迟到的来一条这句的"中间结果"。
    /// 收下它，`display_text` 就会在定稿那句后面再把中间结果拼一遍 ——
    /// 实时输入开着时那段就会被打进目标程序。
    #[test]
    fn late_partial_after_a_final_is_dropped() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Partial, "今天天气不错", Some(1)));
        t.absorb(text(Kind::Final, "今天天气不错。", Some(1)));
        assert_eq!(t.display_text(), "今天天气不错。");

        assert!(
            t.absorb(text(Kind::Partial, "今天天气不错", Some(1)))
                .is_none(),
            "定稿之后的迟到中间结果必须丢掉，且不该回报给界面"
        );
        assert_eq!(
            t.display_text(),
            "今天天气不错。",
            "迟到的中间结果被收下就会打出一份重复的字"
        );
    }

    /// 新句照常往后拼：幂等不能把正常的连续说话吃掉
    #[test]
    fn newer_sentences_still_append() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Final, "第一句。", Some(1)));
        t.absorb(text(Kind::Final, "第二句。", Some(2)));
        t.absorb(text(Kind::Partial, "第三句说了一半", Some(3)));
        assert_eq!(t.display_text(), "第一句。第二句。第三句说了一半");
    }

    /// 比最近定稿更旧的句子再来一条定稿：丢掉（内容已经见过，就是重复上报）。
    /// 宁可丢掉"对旧句的修正"，也不能让同一句在屏幕上出现两遍。
    #[test]
    fn final_for_a_older_sentence_is_dropped() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Final, "第一句。", Some(1)));
        t.absorb(text(Kind::Final, "第二句。", Some(2)));
        assert!(t.absorb(text(Kind::Final, "第一句。", Some(1))).is_none());
        assert_eq!(t.final_text, "第一句。第二句。");
    }

    /// 同一个编号又来了一条定稿（"第二句" → "第二句。"）：替换掉上一段，
    /// 而不是留下「第二句第二句。」
    #[test]
    fn same_id_that_is_a_revision_replaces_the_previous_part() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Final, "第一句。", Some(1)));
        t.absorb(text(Kind::Final, "第二句", Some(2)));
        t.absorb(text(Kind::Final, "第二句。", Some(2)));
        assert_eq!(t.final_text, "第一句。第二句。");
    }

    /// 没有句子编号（豆包/腾讯）时保持原来的"无条件追加"，不能把行为改坏
    #[test]
    fn text_without_ids_keeps_appending() {
        let mut t = Transcript::default();
        t.absorb(text(Kind::Final, "你好。", None));
        t.absorb(text(Kind::Final, "你好。", None));
        assert_eq!(t.final_text, "你好。你好。");
    }
}
