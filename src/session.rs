//! 会话编排：按下开始 → 采集 + 连服务 → 推流 → 松开结束 → 收最终文本 → 打字输出。
//! 独立线程跑 tokio 运行时，UI 线程通过 Shared 快照读取状态。

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::asr::AsrClient;
use crate::config::Config;
use crate::{audio, log, typer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// 按住说话开始
    Start,
    /// 松开结束
    Stop,
    /// 设置页的连通性测试
    Test,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub recording: bool,
    pub text: String,
    pub hint: String,
    pub error: Option<String>,
    pub error_at: Option<Instant>,
    pub last_result: String,
    pub started: Option<Instant>,
}

impl Snapshot {
    pub fn elapsed(&self) -> Duration {
        self.started.map(|t| t.elapsed()).unwrap_or_default()
    }
}

pub struct Shared {
    inner: Mutex<Snapshot>,
    ctx: egui::Context,
}

impl Shared {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            inner: Mutex::new(Snapshot::default()),
            ctx,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        if let Ok(mut guard) = self.inner.lock() {
            f(&mut guard);
        }
        self.ctx.request_repaint();
    }

    /// 把设置窗收起来（录音时用，让焦点回到用户正在用的程序，
    /// 否则打出来的字会落到我们自己的窗口上）
    pub fn hide_settings(&self) {
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        log::log(format!("会话失败：{message}"));
        self.update(|s| {
            s.recording = false;
            s.hint.clear();
            s.error = Some(message);
            s.error_at = Some(Instant::now());
        });
    }

    /// 只报错，不动录音状态。
    /// 实时输入失败时用这个：录音还在继续，界面不该显示成"没在录音"。
    pub fn report(&self, message: impl Into<String>) {
        let message = message.into();
        log::log(format!("提示：{message}"));
        self.update(|s| {
            s.error = Some(message);
            s.error_at = Some(Instant::now());
        });
    }
}

/// 启动会话线程，返回命令发送端
pub fn spawn(cfg: Arc<Mutex<Config>>, shared: Arc<Shared>) -> UnboundedSender<Cmd> {
    let (tx, rx) = unbounded_channel();
    let sender = tx.clone();
    // 线程起不来必须记日志：之后所有命令都会静默发送失败，没有这条日志无从排查
    if let Err(e) = std::thread::Builder::new()
        .name("bbvoxi-session".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::log(format!("创建异步运行时失败：{e}"));
                    return;
                }
            };
            runtime.block_on(worker(cfg, shared, rx));
        })
    {
        log::log(format!("会话线程启动失败：{e}"));
    }
    sender
}

async fn worker(cfg: Arc<Mutex<Config>>, shared: Arc<Shared>, mut rx: UnboundedReceiver<Cmd>) {
    use futures_util::FutureExt;
    while let Some(cmd) = rx.recv().await {
        // 即便某次识别过程中出现意料外的 panic，也只结束这一次会话，
        // 常驻程序继续活着并把错误显示给用户（不要整个进程消失）
        let outcome = std::panic::AssertUnwindSafe(async {
            match cmd {
                Cmd::Start => run_session(&cfg, &shared, &mut rx, false).await,
                Cmd::Test => run_session(&cfg, &shared, &mut rx, true).await,
                Cmd::Stop => {} // 空闲时收到 Stop 忽略即可
            }
        })
        .catch_unwind()
        .await;

        if outcome.is_err() {
            shared.fail("识别过程出现异常，请重试（详情见日志）");
        }
    }
}

async fn run_session(
    cfg: &Arc<Mutex<Config>>,
    shared: &Arc<Shared>,
    rx: &mut UnboundedReceiver<Cmd>,
    test: bool,
) {
    let cfg = match cfg.lock() {
        Ok(c) => c.clone(),
        Err(_) => return,
    };

    if !cfg.credentials_ready() {
        shared.fail("请先在设置中填写完整的服务商凭据");
        return;
    }

    // 快捷键/托盘触发的录音：先把设置窗收起来，焦点回到用户的应用，
    // 这样识别结果才会打进光标所在的位置（测试模式保留窗口，方便看结果）
    if !test {
        shared.hide_settings();
    }

    shared.update(|s| {
        s.recording = true;
        s.text.clear();
        s.hint = "正在连接…".into();
        s.error = None;
        s.error_at = None;
        // 计时器先不启动：见下面"请说话…"处把 started 与 test_limit 对齐的说明
        s.started = None;
    });

    let mut capture = match audio::start() {
        Ok(c) => c,
        Err(e) => {
            shared.fail(format!("{e}"));
            return;
        }
    };

    let mut client = match AsrClient::connect(&cfg).await {
        Ok(c) => c,
        Err(e) => {
            shared.fail(format!("{e:#}"));
            return;
        }
    };

    shared.update(|s| s.hint = "请说话…".into());

    // 记住开始时的前台窗口：录音中途主人切走了，就一个字都别再往新窗口写。
    // 把它交给 typer 自己持有（而不是散在各个调用点），这样 sync/finish 每次
    // 都必须重新上报当前窗口 —— 漏掉校验这件事从"靠自觉"变成"编译不过"。
    let watch = foreground_window();
    // 实时输入：中间结果一到就写进目标程序（被修正时自动回退重打）。
    // 测试模式用 muted：不只是"关掉实时打字"，而是连收尾都绝不允许注入 ——
    // 「结果只显示在窗口里」是软件对主人的承诺。
    let mut typer = if test {
        typer::LiveTyper::muted()
    } else {
        typer::LiveTyper::new(cfg.options.live_typing, watch)
    };

    // 只有「测试识别」自动结束（5 秒）；正常录音完全跟着按键走，按多久录多久，
    // 不做时长限制。
    //
    // 计时必须从**这一刻**起算（麦克风已开、服务已连上、紧接着提示"请说话"）。
    // 之前是在函数开头就算的，连接耗时会被吃进这 5 秒里 —— 网络慢的时候
    // 主人还没开口就到期了，测试窗里一个字都不会有。
    let test_limit = test.then(|| tokio::time::Instant::now() + Duration::from_secs(5));

    // 界面上的计时也从这个时刻起算，和上面那 5 秒同一个起跑线：
    // 否则测试窗口会显示"录音中 00:07"却只录了 5 秒，看着像坏了。
    shared.update(|s| s.started = Some(Instant::now()));

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                // 松开快捷键 = 结束录音
                Some(Cmd::Stop) | None => break,
                // 已经在录音，重复的启动指令忽略
                Some(Cmd::Start) | Some(Cmd::Test) => {}
            },
            frame = capture.rx.recv() => match frame {
                Some(f) => {
                    if let Err(e) = client.send_audio(&f).await {
                        shared.fail(format!("{e:#}"));
                        return;
                    }
                }
                None => break, // 麦克风线程结束
            },
            msg = client.recv() => match msg {
                Some(m) => {
                    if let Some((_, text)) = client.handle(m) {
                        shared.update(|s| s.text = text.clone());
                        // 当前窗口是必填参数：typer 自己会核对"主人有没有切走"
                        if let Err(e) = typer.sync(&text, foreground_window()) {
                            shared.report(format!("{e}"));
                        }
                    }
                    if client.state.done {
                        break;
                    }
                }
                None => break,
            },
            // 正常录音不设时限（None 时这个分支永不就绪）
            _ = async {
                match test_limit {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            } => break,
        }
    }

    // 收尾：把残留音频发完，再发结束指令
    while let Ok(frame) = capture.rx.try_recv() {
        if client.send_audio(&frame).await.is_err() {
            break;
        }
    }
    drop(capture); // 释放麦克风

    shared.update(|s| {
        s.hint = "正在识别…".into();
    });

    if let Err(e) = client.finish().await {
        log::log(format!("发送结束指令失败：{e:#}"));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !client.state.done {
        match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(m)) => {
                if let Some((_, text)) = client.handle(m) {
                    shared.update(|s| s.text = text.clone());
                    // 松手后等最终结果的这段时间，主人同样可能切走窗口，
                    // 所以这里也要过一遍：漏掉它，迟到的结果就会打进别的程序。
                    if let Err(e) = typer.sync(&text, foreground_window()) {
                        shared.report(format!("{e}"));
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {
                log::log("等待最终结果超时（8 秒），使用已收到的内容");
                break;
            }
        }
    }

    let text = client.take_result();
    let asr_error = client.state.last_error.clone();
    drop(client);

    shared.update(|s| {
        s.recording = false;
        s.hint.clear();
        s.last_result = text.clone();
        s.text = text.clone();
    });

    if text.is_empty() {
        // 服务端有明确错误就显示它，否则提示没识别到内容
        match asr_error {
            Some(e) => shared.fail(e),
            None => shared.fail("没有识别到内容，请靠近麦克风再说一次"),
        }
        return;
    }
    if let Some(e) = asr_error {
        log::log(format!("识别过程中收到服务端错误：{e}"));
    }
    log::log(format!("识别完成（{} 字）", text.chars().count()));

    if test {
        // 测试模式：结果只回显在设置窗里。既不能注入（会打进设置窗背后的程序），
        // 也不能收起设置窗 —— 主人正盯着它看结果。
        log::log("测试模式：结果只回显在窗口里，不输入任何字符");
        return;
    }

    // 收尾前的最后一道校验：主人可能在上一条结果之后、这段收尾之前又切走了窗口
    // （等最终结果时尤其容易）。先判一次是为了把顺序理顺 —— 已经确定不输入了，
    // 就不必再多此一举去收起设置窗。
    typer.guard(foreground_window());
    if typer.is_abandoned() {
        // `sent` 里记的是打在**旧窗口**里的字：再做收尾对账，退格会删掉新窗口里的
        // 内容；把整段重打到新窗口同样是打错地方。所以一个字都不输入。
        // 文本本身已经存进 last_result（下次会话开始、错误提示清掉后可见），
        // 日志里也有完整记录。
        log::log("录音途中切换了窗口，已放弃自动输入");
        shared.report("录音途中切换了窗口，为避免打错地方，本次结果没有自动输入");
        return;
    }

    ensure_target_focus(shared).await;
    // 收尾：把最终文本同步到光标处。实时输入开着时通常只差最后几个字，
    // 关掉实时输入时则在这里一次性打出全文。
    //
    // 当前窗口是必填参数：`finish` 会拿它再核对一次。即使上面那次校验将来被谁
    // 删掉，这里也兜得住 —— 不会把补打和退格送进别的程序。
    if let Err(e) = typer.finish(&text, foreground_window()) {
        shared.fail(format!("{e}"));
    }
}

/// 当前前台窗口
#[cfg(windows)]
fn foreground_window() -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then(|| hwnd.0 as isize)
}

#[cfg(not(windows))]
fn foreground_window() -> Option<isize> {
    None
}

/// 注入前确认前台窗口不是我们自己的程序；如果是，就把它藏起来等焦点回去。
/// 不这样做的话，用户打开着设置窗时打字会落到自己窗口上，看起来"什么都没打出来"。
async fn ensure_target_focus(shared: &Shared) {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };

        let is_own = unsafe {
            let foreground = GetForegroundWindow();
            if foreground.is_invalid() {
                false
            } else {
                let mut pid = 0u32;
                GetWindowThreadProcessId(foreground, Some(&mut pid));
                pid == GetCurrentProcessId()
            }
        };
        if is_own {
            log::log("注入前发现前台是本程序窗口，先收起再输入");
            shared.hide_settings();
            // 用异步 sleep：这里跑在 tokio 工作线程上，阻塞它会拖住整个会话
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    #[cfg(not(windows))]
    let _ = shared;
}
