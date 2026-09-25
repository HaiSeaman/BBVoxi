//! 会话编排：按下开始 → 采集 + 连服务 → 推流 → 松开结束 → 收最终文本 → 打字输出。
//! 独立线程跑 tokio 运行时，UI 线程通过 Shared 快照读取状态。

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::asr::AsrClient;
use crate::config::Config;
use crate::{audio, clipboard, log, typer};

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
    /// 会话线程请求「把设置窗显示出来」的标志（见 `show_settings` / `take_show_request`）
    show_request: AtomicBool,
}

impl Shared {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            inner: Mutex::new(Snapshot::default()),
            ctx,
            show_request: AtomicBool::new(false),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        // 锁中毒（上一次拿锁的线程 panic 了）时也把里面的数据取回来继续更新，
        // 并记一条日志。以前是静默 `return` —— 界面会毫无反应，主人根本不知道
        // 出了什么事，且此后所有状态更新都悄悄失效。
        let mut guard = self.inner.lock().unwrap_or_else(|e| {
            log::log("界面状态锁已中毒，取回其中的数据继续更新");
            e.into_inner()
        });
        f(&mut guard);
        self.ctx.request_repaint();
    }

    /// 把设置窗收起来（录音时用，让焦点回到用户正在用的程序，
    /// 否则打出来的字会落到我们自己的窗口上）
    pub fn hide_settings(&self) {
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    /// 请求把设置窗重新显示出来。
    /// 放弃自动输入、改走剪贴板交付时用：主人得看得见"结果已放到剪贴板、
    /// 按 Ctrl+V 粘贴"这句提示，否则他会以为这次识别什么都没出。
    ///
    /// 为什么不能在这里直接发 `Visible(true)`：`App::logic` 在"本程序认为窗口
    /// 是收起的"（`shown == false`：静默启动、收起到托盘）状态下每帧都会补发一次
    /// `Visible(false)`，会把这里的请求当场盖掉 —— 结果就是"想让主人看提示"
    /// 却什么也看不见。所以这里只置一个请求位并唤醒界面，由 UI 线程走正规的
    /// 显示流程（`App::show_settings`：更新 `shown`、搬回屏幕内、延后一帧抢焦点）。
    pub fn show_settings(&self) {
        self.show_request.store(true, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// UI 线程取走「显示设置窗」请求（取走即清零）
    pub fn take_show_request(&self) -> bool {
        self.show_request.swap(false, Ordering::Relaxed)
    }

    fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        log::log(format!("会话失败：{message}"));
        self.update(|s| {
            s.recording = false;
            s.hint.clear();
            s.error = Some(message);
        });
    }

    /// 只报错，不动录音状态。
    /// 实时输入失败时用这个：录音还在继续，界面不该显示成"没在录音"。
    pub fn report(&self, message: impl Into<String>) {
        let message = message.into();
        log::log(format!("提示：{message}"));
        self.update(|s| {
            s.error = Some(message);
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
    // 锁中毒（上一次拿锁的线程 panic 了）不能让会话静默失败：把里面的配置
    // 取回来继续用，并记一条日志，好让主人知道出了点异常。
    let cfg = cfg
        .lock()
        .unwrap_or_else(|e| {
            log::log("配置锁已中毒，取回其中的数据继续");
            e.into_inner()
        })
        .clone();

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

    // 记住开始时的前台窗口：主人中途切走了，就一个字都别再往新窗口写。
    // 把它交给 typer 自己持有（而不是散在各个调用点），这样 sync/finish 每次
    // 都必须重新上报当前窗口 —— 漏掉校验这件事从"靠自觉"变成"编译不过"。
    //
    // 取值必须等焦点**真的离开本程序**：之前是 hide 之后立刻取，中间只隔着
    // 开麦克风和连服务；设置窗还没收起时取到的是我们自己的句柄，接下来第一次
    // guard 就会把主人自己的应用误判成"切换了窗口"，本次结果一个字都打不出去。
    //
    // 测试模式不取：它一个字都不注入，没有"目标窗口"这回事；而此刻设置窗
    // 本来就该在最前面，去等"焦点离开本程序"只会白等 750ms 再记一条无用的警告。
    let watch = if test {
        None
    } else {
        wait_for_foreign_foreground().await
    };
    // 实时输入：中间结果一到就写进目标程序（被修正时自动回退重打）。
    // 测试模式用 muted：不只是"关掉实时打字"，而是连收尾都绝不允许注入 ——
    // 「结果只显示在窗口里」是软件对主人的承诺。
    let mut typer = if test {
        typer::LiveTyper::muted()
    } else {
        typer::LiveTyper::new(cfg.options.live_typing, watch).with_fallback(typer::Fallback {
            paste: cfg.options.clipboard_fallback,
            keep_on_clipboard: cfg.options.keep_on_clipboard,
        })
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

    // 采集流是不是「自己结束」的（rx 关闭）。只有这种情况才需要去看错误原因，
    // 见循环结束后的处理。不能在分支里直接调 capture.error()：rx 正被 select
    // 借用着，借用冲突编译不过。
    let mut mic_ended = false;

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
                    // 推流必须设超时：网络半死时 send_audio 会永久 await，
                    // 于是 Cmd::Stop 再也收不到，会话与麦克风一起卡死 ——
                    // 之后所有热键都失效。超时就按失败收场。
                    match tokio::time::timeout(Duration::from_secs(5), client.send_audio(&f)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            shared.fail(format!("{e:#}"));
                            return;
                        }
                        Err(_) => {
                            shared.fail("发送音频超时（网络不通），已结束本次录音");
                            return;
                        }
                    }
                }
                // 采集线程结束（rx 关闭）：拔出麦克风等错误会让回调置位 stopped，
                // 采集线程随之退出，这里就会收到 None。
                None => {
                    mic_ended = true;
                    break;
                }
            },
            msg = client.recv() => match msg {
                Some(m) => {
                    if let Some((_, text)) = client.handle(m) {
                        shared.update(|s| s.text = text.clone());
                        // 当前窗口是必填参数：typer 自己会核对"主人有没有切走"
                        sync_live(&mut typer, shared, &text);
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

    // 收尾：先把麦克风停掉，再收干残留音频，最后发结束指令。
    //
    // 为什么必须先停麦克风：不停的话采集线程还在以每 100ms 一帧的速度供货，
    // 下面「收干」的 try_recv 就永远有新帧可读 —— 收干会变成「边录边发」，
    // 会话迟迟收不了尾（原实现是在 drain 之后才 drop(capture)，正是这个毛病）。
    //
    // 这里用 capture.stop() 而不是直接 drop(capture)：stop 只让采集线程退出、
    // 释放麦克风；接收端仍在我们手里，缓冲里已录下的帧还能读完，尾巴不丢。
    capture.stop();

    // 给采集线程一点时间收工再收干：它每 50ms 轮询一次停止标志，退出前会把
    // 重采样器里压着的样本、以及不足一帧的残料补发进通道（见 `audio::Feed::push`）。
    // 不留这个间隔，下面的收干循环就很可能抢在补发之前读到"通道已空" ——
    // 尾巴照样丢，等于白补（主人听到的现象就是"最后一个字少一点"）。
    tokio::time::sleep(Duration::from_millis(60)).await;

    // 采集是自己结束的（rx 关闭）且带着错误：如实报「麦克风异常」。
    // 不能含糊地说「没有识别到内容」—— 那会让主人以为是自己没说话，
    // 其实是设备出了问题（被独占、被拔掉……）。
    if mic_ended {
        if let Some(msg) = capture.error() {
            drop(capture);
            shared.fail(format!("麦克风异常：{msg}"));
            return;
        }
    }

    // 收干残留音频：上限 20 帧（约 2 秒）。残留再多也只是缓冲里陈旧的音频，
    // 没必要全发；给上限是为了杜绝「理论上一直有残留」时卡死。
    // 每一步同样加超时，网络不通时不能把会话挂死在这里。
    for _ in 0..20 {
        let Ok(frame) = capture.rx.try_recv() else {
            break;
        };
        match tokio::time::timeout(Duration::from_secs(5), client.send_audio(&frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::log(format!("收尾发送残留音频失败：{e:#}"));
                break;
            }
            Err(_) => {
                log::log("收尾发送残留音频超时（网络不通）");
                break;
            }
        }
    }
    drop(capture); // 释放麦克风与接收端

    shared.update(|s| {
        s.hint = "正在识别…".into();
    });

    if let Err(e) = client.finish().await {
        log::log(format!("发送结束指令失败：{e:#}"));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !client.state.done {
        tokio::select! {
            // 收尾期间主人又按了快捷键。麦克风在同一时刻就已经释放了，这次的
            // 按键**已经录不到**，只能在这里丢掉 —— 留着它们会排进队列，
            // 等本次会话结束后启动一个录到 0 音频的"幽灵会话"，报一句
            // "没有识别到内容"，并且会把刚显示出来的这一句结果从界面上顶掉。
            cmd = rx.recv() => match cmd {
                Some(Cmd::Start) | Some(Cmd::Stop) => {
                    log::log("收尾期间的按键已忽略（此时麦克风已释放，录不到内容）");
                }
                Some(Cmd::Test) => {
                    // 测试请求同样没法插进来执行，但必须说一声 —— 按钮按下去
                    // 什么都不发生比报错更让人困惑
                    shared.report("上一次识别还没结束，请稍后再点「测试识别」");
                }
                None => {}
            },
            msg = tokio::time::timeout_at(deadline, client.recv()) => match msg {
                Ok(Some(m)) => {
                    if let Some((_, text)) = client.handle(m) {
                        shared.update(|s| s.text = text.clone());
                        // 松手后等最终结果的这段时间，主人同样可能切走窗口，
                        // 所以这里也要过一遍：漏掉它，迟到的结果就会打进别的程序。
                        sync_live(&mut typer, shared, &text);
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    log::log("等待最终结果超时（8 秒），使用已收到的内容");
                    break;
                }
            },
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

    ensure_target_focus(shared).await;
    // 收尾：把最终文本同步到光标处。实时输入开着时通常只差最后几个字，
    // 关掉实时输入时则在这里一次性打出全文。
    //
    // 当前窗口是必填参数：`finish` 会拿它再核对一次 —— 主人可能在上一条结果
    // 之后、这段收尾之前又切走了窗口（等最终结果时尤其容易），漏掉校验
    // 就会把补打和退格送进别的程序。
    //
    // `finish` 返回三态，调用方必须分开处理（这正是为了堵住"结果被静默丢弃"）：
    //   Ok(true)  = 文本已交付到目标程序（或本来就无需输入），继续往下走；
    //   Ok(false) = 本次**放弃输入**（主人切走了窗口），结果必须改走剪贴板；
    //   Err(_)    = 注入失败（管理员窗口、被拦……），同样改走剪贴板并把原因报出来。
    match typer.finish(&text, foreground_window()) {
        Ok(true) => {}
        Ok(false) => {
            // `sent` 里记的是打在**原窗口**里的字：此刻做收尾对账，退格会删掉主人
            // 现在这个窗口里的内容；把整段重打到这儿同样是打错地方。所以一个字都不输入。
            //
            // 但结果不能就这么算了 —— 把它放到剪贴板，主人至少还能 Ctrl+V 粘回去。
            // 这正是"切走窗口"这个场景下唯一安全的交付方式：不动别人的窗口，
            // 又把文字交到主人手上（他此刻的焦点就是他要粘的地方）。
            log::log("主人此刻不在原窗口，已放弃自动输入（结果放进剪贴板）");
            shared.show_settings();
            shared.report(hand_off_to_clipboard(
                &text,
                "录音途中切换了窗口，为避免打错地方，本次结果没有自动输入。已复制到剪贴板，按 Ctrl+V 粘贴",
            ));
            return;
        }
        Err(e) => {
            // 注入失败：错误照报，但结果同时放进剪贴板 —— 否则主人只能去设置窗里
            // 手动选中复制。顺手把设置窗显示出来，主人才能看见这句提示。
            shared.show_settings();
            shared.fail(hand_off_to_clipboard(&text, &format!("{e}")));
            return;
        }
    }

    // 「识别结果总留一份到剪贴板」：成功输入也留一份。
    // 这是唯一能覆盖"注入报了成功、字却被目标程序悄悄丢掉"那一档的手段 ——
    // 那种情况我们在客户端根本观测不到，只能保证主人手里始终有一份。
    if cfg.options.keep_on_clipboard {
        match clipboard::write_text(&text) {
            Ok(()) => log::log("已按设置把识别结果留在剪贴板"),
            Err(e) => log::log(format!("把识别结果留到剪贴板失败：{e}")),
        }
    }
}

/// 把结果放到剪贴板，并把提示语补全成主人看得懂的一句话。
///
/// 失败不改提示（写不进去不是主人的问题，日志里有），只如实说明"没复制上"。
fn hand_off_to_clipboard(text: &str, message: &str) -> String {
    match clipboard::write_text(text) {
        Ok(()) => message.to_string(),
        Err(e) => {
            log::log(format!("把结果放进剪贴板失败：{e}"));
            format!("{message}（但剪贴板被占用，没能复制进去）")
        }
    }
}

/// 实时阶段把中间结果同步到光标处。
///
/// 失败只提示、不打断会话：录音还在继续，界面不该显示成"没在录音"。
/// 实时阶段和收尾等结果的阶段各要一次，抽出来免得两处写法走偏
/// （前台窗口是必填参数，见 `LiveTyper::guard`）。
fn sync_live(typer: &mut typer::LiveTyper, shared: &Shared, text: &str) {
    if let Err(e) = typer.sync(text, foreground_window()) {
        shared.report(format!("{e}"));
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

/// 这个窗口属于我们自己的进程吗
#[cfg(windows)]
fn is_own_window(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid));
    }
    pid == unsafe { GetCurrentProcessId() }
}

#[cfg(not(windows))]
fn is_own_window(_hwnd: isize) -> bool {
    false
}

/// 等焦点离开本程序，然后返回那时的前台窗口作为本次会话的目标窗口。
///
/// 为什么必须等：`hide_settings()` 只是把"隐藏"这个请求投给 egui，
/// 并不能保证焦点立刻回到主人的应用。之前紧接着就取前台窗口，于是设置窗
/// 还压在最上面时取到的就是**我们自己的句柄**；接下来第一次 `guard`
/// 就会把主人自己的应用当成"中途切过来的窗口"，本次结果一个字都打不出去。
///
/// 为什么要有个上限：主人未必真有个程序在等焦点（比如他刚关掉设置窗、
/// 桌面上什么都没有）。等不到就返回 `None` —— `LiveTyper` 在句柄未知时
/// 不做切换判定，输入照常，宁可"不设防"也不"乱设防"。
async fn wait_for_foreign_foreground() -> Option<isize> {
    const ATTEMPTS: usize = 25; // 最多 750ms
    for _ in 0..ATTEMPTS {
        match foreground_window() {
            Some(h) if !is_own_window(h) => return Some(h),
            _ => tokio::time::sleep(Duration::from_millis(30)).await,
        }
    }
    log::log("等了 750ms 焦点仍未离开本程序，本次不记录目标窗口");
    None
}

/// 注入前确认前台窗口不是我们自己的程序；如果是，就把它藏起来等焦点回去。
/// 不这样做的话，用户打开着设置窗时打字会落到自己窗口上，看起来"什么都没打出来"。
async fn ensure_target_focus(shared: &Shared) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

        let foreground = unsafe { GetForegroundWindow() };
        let is_own = !foreground.is_invalid() && is_own_window(foreground.0 as isize);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 「显示设置窗」必须是**一次性**的请求位：UI 线程取走后要清零。
    /// 少了这个语义，会话线程每请求一次、界面就会被反复强制显示出来
    /// （主人会看到窗口在他没点的时候自己跳出来）。
    #[test]
    fn show_request_is_taken_exactly_once() {
        let shared = Shared::new(egui::Context::default());
        assert!(!shared.take_show_request(), "初始不该有待处理的请求");
        shared.show_settings();
        assert!(shared.take_show_request(), "置位后应该能取到");
        assert!(!shared.take_show_request(), "取走之后必须清零");
    }
}
