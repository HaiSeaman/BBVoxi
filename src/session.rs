//! 会话编排：按下开始 → 采集 + 连服务 → 推流 → 松开结束 → 收最终文本 → 打字输出。
//! 独立线程跑 tokio 运行时，UI 线程通过 Shared 快照读取状态。

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::asr::AsrClient;
use crate::config::Config;
use crate::{audio, clipboard, log, typer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 按住说话开始
    Start,
    /// 松开结束
    Stop,
    /// 设置页的连通性测试
    Test,
    /// 启动阶段的子系统失败（目前是全局键盘钩子没装上）。
    ///
    /// 为什么走这条通道：键盘钩子那边不碰界面状态，而会话线程正好拿着 `Shared`，
    /// 由它统一写进快照 —— 托盘图标、状态胶囊、设置窗里的那行字就会一起亮起来，
    /// 而不是"看起来一切正常、按键全都没反应"（报告第 8 条）。
    StartupFailure(String),
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub recording: bool,
    pub text: String,
    pub hint: String,
    pub error: Option<String>,
    /// 一次**不算错误**的提示（例如"结果已放进剪贴板"）。
    /// 与 `error` 分开是为了界面上的颜色与托盘提示分开：这类情况不是故障，
    /// 只是这次没能自动输入而已。
    pub notice: Option<String>,
    /// 上一次结果**确实**留在剪贴板里了吗（写完回读核对过）。
    ///
    /// 界面靠它决定要不要说"这份结果也留在了剪贴板"—— 说错了比不说更糟：
    /// 主人照着去 Ctrl+V 却什么也粘不出来，正是他报的那个"提示说复制好了、
    /// 却找不到"。所以这个标记只由**核对通过**的那一次写入置位。
    pub kept_on_clipboard: bool,
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

    /// 结果改走剪贴板时**只提示、不弹窗**。
    ///
    /// 之前这里是把设置窗顶到最前面（`ViewportCommand::Visible(true)`），理由是
    /// "主人得看得见这句话"。可主人此刻正在别的程序里干活：窗口自己跳出来既打断他，
    /// 又会被当成"软件乱弹"（他报的就是这个）。所以提示换到不打扰的地方：
    /// 托盘图标与悬浮文字（见 `main.rs::sync_tray`）、窗口里的那行字（他愿意看时
    /// 再看）、以及日志。文字放在 `notice` 而不是 `error`：这不是故障。
    pub fn notice(&self, message: impl Into<String>) {
        let message = message.into();
        log::log(format!("提示（不弹窗）：{message}"));
        self.update(|s| {
            s.notice = Some(message);
        });
    }

    /// 一次会话开始时的状态复位。抽成方法而不是就地写一段 `update`：
    /// 这样"上一次的提示要退场"这条契约可以在单测里直接钉住。
    ///
    /// `started` 特意留空：计时要从**麦克风已开、服务已连上**那一刻起算
    /// （见 `run_session` 里把 started 与 test_limit 对齐的说明），在会话开头
    /// 就算时间会把连接耗时吃进那 5 秒测试时限里。
    pub fn begin_session(&self) {
        self.update(|s| {
            s.recording = true;
            s.text.clear();
            s.hint = "正在连接…".into();
            s.error = None;
            // 上一次留下的"结果已放进剪贴板"提示这时就该退场了：
            // 它挂着的意义是"主人还没去粘贴"，而不是长期状态
            s.notice = None;
            s.kept_on_clipboard = false;
            s.started = None;
        });
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

/// 后台识别线程起不来时的那句话（线程建不起来 / 运行时建不起来，两条路共用）。
///
/// 抽出来是为了能单测：这条上报影响主人的判断（是不是软件坏了、要不要重启），
/// 措辞和"必须真的写进界面状态"这两件事都值得钉住。
fn report_session_start_failure(shared: &Shared, why: impl std::fmt::Display) {
    let msg = format!("后台识别线程启动失败（{why}）：快捷键不会有任何反应，请重启程序");
    log::log(&msg);
    shared.fail(msg);
}

/// 启动会话线程，返回命令发送端。
///
/// 线程（或它内部的 tokio 运行时）起不来时，**界面必须看得见**：这条链一断，
/// 之后按快捷键、点测试、托盘命令全都石沉大海，而窗口一切正常 ——
/// 主人只会觉得"这软件坏了，按了没反应"（报告第 2 条：静默死亡）。
pub fn spawn(cfg: Arc<Mutex<Config>>, shared: Arc<Shared>) -> UnboundedSender<Cmd> {
    let (tx, rx) = unbounded_channel();
    let sender = tx.clone();
    let for_thread = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("bbvoxi-session".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    // 线程起来了、运行时建不出来：同样是"按什么键都没反应"，
                    // 一样要报到界面上（这里还拿着 shared，能直接写进快照）
                    report_session_start_failure(&for_thread, e);
                    return;
                }
            };
            runtime.block_on(worker(cfg, for_thread, rx));
        });
    if let Err(e) = spawned {
        // 线程都没起来：发送端照旧返回（App 的结构不用改），但消息已经没人收了 ——
        // 必须让主人看见，而不是只写进日志
        report_session_start_failure(&shared, e);
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
                Cmd::Start => {
                    // 「谁按了快捷键」这类日志记在这里、而不是键盘钩子里：
                    // 低级键盘钩子必须毫秒级返回，里面做文件 I/O 一旦卡住（杀软、
                    // 磁盘忙），Windows 会**悄悄**把钩子摘掉 —— 表现就是"快捷键突然
                    // 全都没反应、Win 键开始乱弹开始菜单"，而且日志里什么都没有。
                    log::log("开始录音（收到启动指令）");
                    run_session(&cfg, &shared, &mut rx, false).await
                }
                Cmd::Test => {
                    log::log("测试识别（设置窗）");
                    run_session(&cfg, &shared, &mut rx, true).await
                }
                // 空闲时收到 Stop 忽略即可（录音中的 Stop 由 run_session 自己的
                // 循环收走，不会落到这里）
                Cmd::Stop => log::log("结束录音指令（当前没在录音，已忽略）"),
                // 启动阶段的子系统失败（比如全局键盘钩子没装上）：写进界面状态 ——
                // 托盘图标、状态胶囊、设置窗里的那行字会一起亮起来，主人不用猜
                // "为什么按快捷键没反应"（报告第 8 条）。上报方已经记过日志，这里不重复记。
                Cmd::StartupFailure(msg) => shared.fail(msg),
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

    shared.begin_session();

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
        typer::LiveTyper::new(cfg.options.live_typing, watch)
            .with_paste_fallback(cfg.options.clipboard_fallback)
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

    // 会话是不是中途失败的。**失败不能直接 return** —— 那会跳过"给服务端发结束
    // 指令、松手后等最终结果的窗口、发关闭帧"三件事，而服务端很可能已经把主人
    // 说的那句定稿了：结果取不回来，「最近识别」就停在上一次的内容，
    // 主人手里只剩半句中间结果（报告第 1 条）。所以失败只记原因、跳出循环，
    // 收尾走**和正常路径完全相同**的那一套。
    let mut failure: Option<String> = None;
    // 推流那一步失败的标志：链路本身已经不可信，后面再发结束帧只会白等一个超时
    let mut link_broken = false;

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                // 松开快捷键 = 结束录音
                Some(Cmd::Stop) => {
                    log::log("结束录音（松开了快捷键，或从托盘点了停止）");
                    break;
                }
                None => break,
                // 已经在录音，重复的启动指令忽略
                Some(Cmd::Start) | Some(Cmd::Test) => {}
                // 启动阶段的子系统失败随时可能到（正常是刚开机那会儿）：
                // 这里只提示、不打断正在进行的这次录音
                Some(Cmd::StartupFailure(msg)) => shared.report(msg),
            },
            frame = capture.rx.recv() => match frame {
                Some(f) => {
                    // 推流必须设超时：网络半死时 send_audio 会永久 await，
                    // 于是 Cmd::Stop 再也收不到，会话与麦克风一起卡死 ——
                    // 之后所有热键都失效。超时就按失败收场。
                    match tokio::time::timeout(Duration::from_secs(5), client.send_audio(&f)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            failure = Some(format!("{e:#}"));
                            link_broken = true;
                            break;
                        }
                        Err(_) => {
                            failure = Some("发送音频超时（网络不通），已结束本次录音".into());
                            link_broken = true;
                            break;
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
                // 读线程结束 = 连接断了。**不能当成正常收场**：那样收尾会继续
                // 给一条死链路发结束帧、等满窗口，最后还可能报一句误导的
                // "没有识别到内容，请靠近麦克风再说一次"（其实是连接没了）。
                None => {
                    if let Ending::LinkBroken(reason) = ending_after_reader_closed(
                        client.state.done,
                        client.state.last_error.is_some(),
                    ) {
                        failure.get_or_insert_with(|| reason.to_string());
                        link_broken = true;
                    }
                    break;
                }
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

    // 等采集线程把尾巴补发完再收干：它每 50ms 轮询一次停止标志，退出前会把
    // 重采样器里压着的样本、以及不足一帧的残料补发进通道（见 `audio::Feed::push`）。
    //
    // 为什么是"等标志"而不是睡一个固定时长：蓝牙免提、部分 USB 声卡的回调周期能到
    // 100ms 以上，固定睡 60ms 会在回调到来之前就把流 drop 掉 —— 尾帧再也没机会补发，
    // 主人松手前那几个字就这么丢了（现象就是"最后一个字少一点"）。
    // 上限 300ms，绝不为了尾巴把收尾卡死。
    let tail_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while !capture.tail_flushed() && tokio::time::Instant::now() < tail_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 采集是自己结束的（rx 关闭）且带着错误：把「麦克风异常」记成失败原因。
    // 不能含糊地说「没有识别到内容」—— 那会让主人以为是自己没说话，
    // 其实是设备出了问题（被独占、被拔掉……）。这里**不 return**：主人刚说的
    // 那句话很可能还在服务端那头等着被取回来（见 `failure` 的说明）。
    if mic_ended {
        if let Some(msg) = capture.error() {
            failure.get_or_insert_with(|| format!("麦克风异常：{msg}"));
        }
    }

    // 收干残留音频：上限 20 帧（约 2 秒）。残留再多也只是缓冲里陈旧的音频，
    // 没必要全发；给上限是为了杜绝「理论上一直有残留」时卡死。
    // 每一步同样加超时，网络不通时不能把会话挂死在这里。
    //
    // 链路已经坏掉时整段跳过：再往服务端灌只会白等一个 5 秒超时，而主人已经在等了。
    if !link_broken {
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
    }
    drop(capture); // 释放麦克风与接收端

    shared.update(|s| {
        s.hint = "正在识别…".into();
    });

    // 给服务端发结束指令 —— 失败时这一步**更要发**：服务端手里那句往往已经定稿，
    // 发了它才会把最终结果吐出来。只有推流那一步明确失败（链路不可信）时才跳过，
    // 免得白等一个 5 秒超时。
    if !link_broken {
        if let Err(e) = client.finish().await {
            log::log(format!("发送结束指令失败：{e:#}"));
        }
    }

    let deadline = tokio::time::Instant::now() + final_wait(link_broken);
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
                Some(Cmd::StartupFailure(msg)) => shared.report(msg),
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
    // 主动发一条关闭帧再放手：让服务端看到的是「正常关闭」而不是异常断开
    // （`close()` 以前是个没人调用的死函数，收尾时补上这次调用它才有意义）。
    // 失败只记日志：结果已经拿到了，收尾礼仪不该影响主人这次的字。
    if let Err(e) = client.close().await {
        log::log(format!("发送关闭帧失败（不影响本次识别结果）：{e:#}"));
    }
    drop(client);

    shared.update(|s| {
        s.recording = false;
        s.hint.clear();
        // 中途失败时这一步格外要紧：以前失败直接 return，「最近识别」永远停在
        // 上一次的内容上，主人以为这次白说了
        s.last_result = text.clone();
        s.text = text.clone();
    });

    if let Some(e) = &asr_error {
        log::log(format!("识别过程中收到服务端错误：{e}"));
    }

    if let Some(reason) = failure {
        // 会话中途失败：**结果照样交付**（能打进光标处就打，打不了就进剪贴板），
        // 并把"为什么失败 + 结果在哪"合成一句话报出来
        deliver_after_failure(shared, &cfg, &mut typer, test, &text, &reason).await;
        return;
    }

    if text.is_empty() {
        // 服务端有明确错误就显示它，否则提示没识别到内容
        match asr_error {
            Some(e) => shared.fail(e),
            None => shared.fail("没有识别到内容，请靠近麦克风再说一次"),
        }
        return;
    }
    log::log(format!("识别完成（{} 字）", text.chars().count()));

    if test {
        // 测试模式：结果只回显在设置窗里。既不能注入（会打进设置窗背后的程序），
        // 也不能收起设置窗 —— 主人正盯着它看结果。
        log::log("测试模式：结果只回显在窗口里，不输入任何字符");
        return;
    }

    // 收尾：把最终文本交付出去（能打进光标处就打，打不了就进剪贴板）。
    //
    // 交付判断与**失败收尾共用同一份实现**（`deliver_result`）：实时输入开着时
    // 通常只差最后几个字，关掉实时输入时则在这里一次性打出全文 —— 两种收尾
    // 在这件事上的规则必须一致，只有"怎么把结果说给主人听"不同。
    match deliver_result(shared, &cfg, &mut typer, &text).await {
        Delivery::Typed { clipboard } => match clipboard {
            // 没开「留一份」设置：不用多说
            None => {}
            Some(Ok(())) => log::log("识别结果已留在剪贴板（随时可 Ctrl+V）"),
            // 留不住要说一声（但不弹窗）：主人以为剪贴板里有、去粘却粘不出来最气人
            Some(Err(reason)) => shared.notice(format!(
                "本次结果没能留在剪贴板（{reason}）：文字在设置窗的「最近识别」里，可手动复制"
            )),
        },
        // 一个字都没输入（主人切走了窗口）：此刻把设置窗顶到最前面既打断他、
        // 又会被当成"软件自己乱跳"（他报的就是这个）。所以**绝不弹窗**，
        // 提示走托盘、日志与窗口里那行字 —— 他愿意看的时候再看。
        Delivery::HandedOff(notice) => {
            log::log("主人此刻不在原窗口，已放弃自动输入（结果放进剪贴板）");
            shared.notice(notice);
        }
        // 注入失败：错误照报，但结果已经放进剪贴板了（理由同上）
        Delivery::Failed(message) => shared.fail(message),
    }
}

/// 放弃自动输入时给主人看的那句话（含"到底复制成功了没有"）。
///
/// 成功与失败必须是**两句不同的话**：以前不管成没成都写"已复制到剪贴板"，
/// 主人照着去 Ctrl+V 却什么也粘不出来 —— 这正是他报的那个 bug。
fn hand_off_message(shared: &Shared, text: &str) -> String {
    const HEAD: &str = "录音途中切换了窗口，为避免打错地方，本次结果没有自动输入。";
    match put_on_clipboard(shared, text) {
        None => format!("{HEAD}已复制到剪贴板，按 Ctrl+V 粘贴"),
        Some(reason) => format!("{HEAD}但没能复制到剪贴板（{reason}），可在设置窗里手动复制"),
    }
}

/// 注入失败时的后缀：只补"复制结果如何"，错误本身由调用方照原样报出来。
fn hand_off_suffix(shared: &Shared, text: &str) -> String {
    match put_on_clipboard(shared, text) {
        None => "（结果已放进剪贴板，可 Ctrl+V 粘贴）".to_string(),
        Some(reason) => format!("（结果也未能放进剪贴板：{reason}）"),
    }
}

/// 剪贴板交付的**唯一入口**：写进去、回读核对、把"到底留住了没有"如实写进快照。
///
/// 返回 `None` = 整段都写进去了且回读对得上；`Some(原因)` = 没能可靠地留在剪贴板。
/// 为什么收成一处：留一份（收尾）与改走剪贴板（切走窗口/注入被拒）以前各写了一份
/// 同样的三分支，结论还容易走偏；而界面现在还要靠 `kept_on_clipboard` 决定要不要
/// 说"这份结果也留在了剪贴板"，更不该让三处各自判断。
fn put_on_clipboard(shared: &Shared, text: &str) -> Option<String> {
    let outcome = clipboard::write_text_verified(text);
    let kept = matches!(outcome, Ok(true));
    shared.update(|s| s.kept_on_clipboard = kept);
    match outcome {
        Ok(true) => None,
        Ok(false) => {
            log::log("剪贴板回读与写入不一致：结果可能被别的程序改写了");
            Some("剪贴板可能已被其他程序改写".into())
        }
        Err(e) => {
            log::log(format!("把结果放进剪贴板失败：{e}"));
            Some(format!("剪贴板被占用（{e}）"))
        }
    }
}

/// 读线程结束时该怎么定性（纯函数，便于单测）。
///
/// `done`（服务端已经明确结束）或 `last_error`（服务端报了错）都是有交代的收场；
/// 两者都没有却读到连接结束，那就是**被中断了** —— 必须按失败处理，
/// 否则收尾会对着一条死链路干等，最后还可能报一句误导的"没有识别到内容"。
enum Ending {
    Normal,
    LinkBroken(&'static str),
}

fn ending_after_reader_closed(done: bool, has_error: bool) -> Ending {
    if done || has_error {
        Ending::Normal
    } else {
        Ending::LinkBroken("连接被服务端中断")
    }
}

/// 松手后等"最终结果"的窗口（正常路径）
const FINAL_WAIT: Duration = Duration::from_secs(8);

/// 推流那一步已经失败时等最终结果的窗口（链路不可信，不陪着一起干等）
const BROKEN_LINK_FINAL_WAIT: Duration = Duration::from_secs(2);

/// 收尾等最终结果要等多久。
///
/// 正常 8 秒：足够服务端把最后一句定稿吐回来。
/// 推流已经失败时收短到 2 秒：链路多半已经断了，让主人对着"正在识别…"干等满
/// 8 秒毫无意义；而**麦克风故障时网络是好的**，照常等满 8 秒 ——
/// 服务端正要把那句定稿送出来（报告第 1 条：等最终结果这一步绝不能跳过）。
fn final_wait(link_broken: bool) -> Duration {
    if link_broken {
        BROKEN_LINK_FINAL_WAIT
    } else {
        FINAL_WAIT
    }
}

/// 一次收尾交付的结果（正常收尾与失败收尾都靠它把话说给主人听）
enum Delivery {
    /// 文本已经打进目标程序（光标处），或本来就无需输入。
    /// `clipboard` 说的是"要不要/有没有在剪贴板再留一份"：
    /// `None` = 主人没开这个设置；`Some(Ok(()))` = 留住了；`Some(Err(原因))` = 没留住。
    Typed {
        clipboard: Option<Result<(), String>>,
    },
    /// 本次**放弃输入**（主人切走了窗口）：一个字都没打，整段放进剪贴板。
    /// 里面那句话已经把"到底复制成功没有"说清楚了（见 `hand_off_message`）。
    HandedOff(String),
    /// 注入失败（管理员窗口、被拦……）：错误原因 + "结果在哪"，同样已经放进剪贴板。
    Failed(String),
}

/// 把最终文本交付出去：能打进光标处就打，打不进去就进剪贴板。
///
/// **正常收尾和失败收尾共用这一份**（口径只写一处，两边的说法才不会走偏）。
/// `finish` 返回三态，这里逐态处理 —— 这正是为了堵住"结果被静默丢弃"：
///   `Ok(true)`  文本已交付（或本来就无需输入）；
///   `Ok(false)` 本次放弃输入（主人切走了窗口）；
///   `Err(_)`    注入失败（管理员窗口、被拦……）。
///
/// 当前窗口是必填参数：`finish` 会拿它再核对一次 —— 主人可能在上一条结果之后、
/// 这段收尾之前又切走了窗口（等最终结果时尤其容易），漏掉校验就会把补打和退格
/// 送进别的程序。
async fn deliver_result(
    shared: &Shared,
    cfg: &Config,
    typer: &mut typer::LiveTyper,
    text: &str,
) -> Delivery {
    ensure_target_focus(shared).await;
    match typer.finish(text, foreground_window()) {
        Ok(true) => {
            // 「识别结果留一份到剪贴板」（默认开，见 `config::Options`）。
            //
            // 这是唯一能覆盖"注入报了成功、字却被目标程序悄悄丢掉"那一档的手段：
            // 那种情况客户端**根本观测不到** —— `SendInput` 只报告事件已入队，
            // 目标收下再丢掉我们看不见。所以只能保证主人手里始终有一份。
            let clipboard = cfg.options.keep_on_clipboard.then(|| {
                match put_on_clipboard(shared, text) {
                    None => Ok(()),
                    Some(why) => Err(why),
                }
            });
            Delivery::Typed { clipboard }
        }
        // `sent` 里记的是打在**原窗口**里的字：此刻做收尾对账，退格会删掉主人现在
        // 这个窗口里的内容；把整段重打到这儿同样是打错地方。所以一个字都不输入，
        // 改成"整段放进剪贴板"—— 这是此刻唯一安全的交付方式（不动别人的窗口，
        // 又把文字交到主人手上，他此刻的焦点就是他要粘的地方）。
        Ok(false) => Delivery::HandedOff(hand_off_message(shared, text)),
        // 注入失败：错误照报，但结果同时放进剪贴板 —— 否则主人只能去设置窗里手动选中复制。
        Err(e) => Delivery::Failed(format!("{e}{}", hand_off_suffix(shared, text))),
    }
}

/// 会话中途失败时的收尾：**已经识别到的内容一个字都不能丢**，并且把
/// "为什么失败"和"结果放在哪"合成一句话告诉主人。
///
/// 旧实现在三条失败路径（麦克风异常 / 推流失败 / 推流超时）上直接 `return`：
/// 既没去取服务端已经定稿的那句，也没把结果交付出去 ——「最近识别」停在上一次，
/// 主人手里只剩半句中间结果（报告第 1 条）。交付动作与正常收尾共用
/// [`deliver_result`]，这里只额外负责"把失败原因说清楚"。
async fn deliver_after_failure(
    shared: &Shared,
    cfg: &Config,
    typer: &mut typer::LiveTyper,
    test: bool,
    text: &str,
    reason: &str,
) {
    if text.trim().is_empty() {
        // 什么都没识别到：如实报失败原因（比含糊的"没有识别到内容"有用得多）
        shared.fail(reason.to_string());
        return;
    }
    let count = text.chars().count();
    if test {
        // 测试模式：结果只回显在设置窗里，一个字都不许往外打
        shared.fail(format!(
            "{reason}（已识别到的 {count} 字在窗口的「最近识别」里）"
        ));
        return;
    }
    match deliver_result(shared, cfg, typer, text).await {
        Delivery::Typed { clipboard } => {
            let extra = match clipboard {
                None => String::new(),
                Some(Ok(())) => "，并留了一份到剪贴板".to_string(),
                Some(Err(why)) => format!("（没能留进剪贴板：{why}）"),
            };
            log::log(format!("会话中途失败，已把识别到的 {count} 字输入到光标处"));
            shared.fail(format!("{reason}；识别到的 {count} 字已输入到光标处{extra}"));
        }
        Delivery::HandedOff(notice) => {
            log::log("会话中途失败，且主人不在原窗口：结果改走剪贴板");
            shared.fail(format!("{reason}；{notice}"));
        }
        Delivery::Failed(message) => {
            log::log(format!("会话中途失败，且注入被拒：{message}"));
            shared.fail(format!("{reason}；{message}"));
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

/// 此刻的前台窗口是不是我们自己（没有前台窗口时算"不是"）。
///
/// 抽出来是因为"要不要把字交给它"这件事在两个地方要判断（等焦点离开本程序、
/// 注入前再确认一次），以前各自写了一遍 `GetForegroundWindow` + 判归属 + 判无效句柄。
fn foreground_is_own() -> bool {
    foreground_window().is_some_and(is_own_window)
}

/// 注入前确认前台窗口不是我们自己的程序；如果是，就把它藏起来等焦点回去。
/// 不这样做的话，用户打开着设置窗时打字会落到自己窗口上，看起来"什么都没打出来"。
async fn ensure_target_focus(shared: &Shared) {
    if foreground_is_own() {
        log::log("注入前发现前台是本程序窗口，先收起再输入");
        shared.hide_settings();
        // 用异步 sleep：这里跑在 tokio 工作线程上，阻塞它会拖住整个会话
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结果改走剪贴板时必须**只提示、不弹窗**，而且提示要落在 `notice` 而不是 `error`。
    ///
    /// 这条守的是主人报的"这个提示会弹出软件窗口"：
    /// - 不能走 `error` —— 那不是故障（界面会红着脸报错、托盘也会变成出错态）；
    /// - 提示本身要留在快照里：主人打开设置窗时得能看到这句话。
    ///
    /// 更要紧的是：`Shared` 里**不该再有任何"请求显示窗口"的通道**（以前正是它把
    /// 设置窗顶到最前面）。`show_settings` / `take_show_request` 连同那个原子标志
    /// 已经删掉了 —— 这条用例把"只提示"这个契约钉在明面上。
    #[test]
    fn clipboard_handoff_only_notices_it_never_asks_for_the_window() {
        let shared = Shared::new(egui::Context::default());
        shared.notice("已复制到剪贴板，按 Ctrl+V 粘贴");
        let snap = shared.snapshot();
        assert_eq!(
            snap.notice.as_deref(),
            Some("已复制到剪贴板，按 Ctrl+V 粘贴"),
            "提示要留得下来，主人打开窗口时看得到"
        );
        assert!(snap.error.is_none(), "这不是故障，不该走 error 通道");
        assert!(!snap.recording, "提示不该把状态改回录音中");
    }

    /// 「这份结果真的留在剪贴板里了」这个标记只由**核对通过**的写入置位：
    /// 界面靠它决定要不要说那句话，说错了主人就会对着空的剪贴板干瞪眼。
    ///
    /// 这里只测标记的语义（置位/复位）—— 真去写剪贴板的那条路在 `clipboard`
    /// 模块里由 `#[ignore]` 的手测覆盖（单测不许动主人真实的剪贴板）。
    #[test]
    fn kept_on_clipboard_flag_is_explicit_and_cleared_per_session() {
        let shared = Shared::new(egui::Context::default());
        assert!(
            !shared.snapshot().kept_on_clipboard,
            "初始状态不许声称'已经留了一份'"
        );
        shared.update(|s| s.kept_on_clipboard = true);
        assert!(shared.snapshot().kept_on_clipboard);
        shared.begin_session();
        assert!(
            !shared.snapshot().kept_on_clipboard,
            "开新会话时上一次的'留住了'必须退场，否则界面会拿旧结论说事"
        );
    }

    /// 上一次的剪贴板提示必须在下次录音开始时退场，
    /// 否则托盘会一直挂着"已复制到剪贴板"，主人以为这次也没输入
    #[test]
    fn begin_session_clears_the_previous_notice() {
        let shared = Shared::new(egui::Context::default());
        shared.notice("已复制到剪贴板，按 Ctrl+V 粘贴");
        shared.begin_session();
        let snap = shared.snapshot();
        assert!(snap.recording, "新会话要立刻进入录音状态");
        assert!(
            snap.notice.is_none(),
            "上一次的提示没清掉，托盘会一直挂着旧话"
        );
        assert!(snap.error.is_none());
        assert!(snap.started.is_none(), "计时要从麦克风与服务都就绪后才开始");
    }

    /// 失败收尾：**已经识别到的内容一个字都不能丢**。
    ///
    /// 回归（报告第 1 条）：旧实现在会话中途失败（麦克风异常 / 推流失败 / 推流超时）
    /// 时直接 `return` —— 跳过了取回服务端最终结果这一步，「最近识别」永远停在上
    /// 一次的内容上，主人手里只剩半句中间结果。现在失败也要把结果交付出去，
    /// 并把"识别到了多少字"说清楚。
    ///
    /// 用 `test = true` 跑：这条路不会注入、不会碰剪贴板（测试模式对主人的承诺
    /// 就是"一个字都不往外打"），所以可以放心在单测里走。
    #[tokio::test]
    async fn a_failed_session_still_reports_what_was_recognized() {
        let shared = Shared::new(egui::Context::default());
        let mut cfg = Config::default();
        cfg.options.keep_on_clipboard = false; // 别让测试去动真实剪贴板
        let mut t = typer::LiveTyper::muted();
        deliver_after_failure(
            &shared,
            &cfg,
            &mut t,
            true,
            "你好世界",
            "麦克风异常：设备被独占",
        )
        .await;
        let snap = shared.snapshot();
        let msg = snap.error.expect("失败原因必须报出来");
        assert!(msg.contains("麦克风异常"), "原因要原样带上：{msg}");
        assert!(msg.contains("4 字"), "要告诉主人这次识别到了多少字：{msg}");
        assert!(!snap.recording, "失败之后不能还显示成'录音中'");
    }

    /// 失败收尾：什么都没识别到时，报的就是失败原因本身 ——
    /// 不能含糊地说"没有识别到内容"，那会让主人以为是自己没说话
    /// （其实是设备/网络出了问题）
    #[tokio::test]
    async fn a_failed_session_without_text_reports_the_reason_itself() {
        let shared = Shared::new(egui::Context::default());
        let cfg = Config::default();
        let mut t = typer::LiveTyper::muted();
        deliver_after_failure(
            &shared,
            &cfg,
            &mut t,
            true,
            "   ",
            "发送音频超时（网络不通），已结束本次录音",
        )
        .await;
        assert_eq!(
            shared.snapshot().error.as_deref(),
            Some("发送音频超时（网络不通），已结束本次录音")
        );
    }

    /// 等最终结果的窗口：链路坏掉时收短，但**绝不能是 0** ——
    /// 服务端可能已经把最后一句定稿了，这个窗口是我们唯一能把它取回来的机会
    #[test]
    fn final_wait_shrinks_but_never_vanishes_when_the_link_is_broken() {
        assert_eq!(final_wait(false), FINAL_WAIT);
        assert_eq!(final_wait(true), BROKEN_LINK_FINAL_WAIT);
        assert!(!final_wait(true).is_zero(), "窗口再短也得留出取结果的机会");
        assert!(
            final_wait(true) < final_wait(false),
            "链路已经坏掉时不该陪它干等满 8 秒"
        );
    }

    /// 交付判断只有一份实现：安静打字器（测试模式）+ 关掉"留一份"时，
    /// 结果应报成"已交付"，而且**不碰剪贴板**（单测绝不许动主人真实的剪贴板）。
    #[tokio::test]
    async fn deliver_result_reports_typed_without_touching_the_clipboard() {
        let shared = Shared::new(egui::Context::default());
        let mut cfg = Config::default();
        cfg.options.keep_on_clipboard = false; // 别让测试去动真实剪贴板
        let mut t = typer::LiveTyper::muted();
        let delivery = deliver_result(&shared, &cfg, &mut t, "你好世界").await;
        match delivery {
            Delivery::Typed { clipboard } => {
                assert!(clipboard.is_none(), "没开「留一份」时不该去写剪贴板");
            }
            _ => panic!("测试模式的收尾必须是「已交付」，不能是放弃输入或失败"),
        }
        assert!(shared.snapshot().error.is_none(), "交付成功不该报错");
    }

    /// 读线程结束时怎么定性（回归，评审发现）：连接被中断以前被当成正常收场 ——
    /// 收尾会对着一条死链路干等，最后还可能报一句误导的"没有识别到内容，
    /// 请靠近麦克风再说一次"（其实是连接没了）。
    #[test]
    fn a_silent_connection_drop_counts_as_a_failure() {
        assert!(
            matches!(ending_after_reader_closed(true, false), Ending::Normal),
            "服务端明确结束 = 有交代的收场"
        );
        assert!(
            matches!(ending_after_reader_closed(false, true), Ending::Normal),
            "服务端报过错 = 也有交代"
        );
        match ending_after_reader_closed(false, false) {
            Ending::LinkBroken(reason) => {
                assert!(reason.contains("中断"), "要说清是连接断了：{reason}");
            }
            Ending::Normal => panic!("什么都没交代就断线，绝不能当成正常收场"),
        }
    }

    /// 后台识别线程（或它内部的运行时）起不来时，**界面必须看得见**。
    ///
    /// 回归（报告第 2 条）：这条链断了之后，按快捷键、点测试、托盘命令全都
    /// 石沉大海，而窗口一切正常 —— 主人只会觉得"软件坏了，按了没反应"。
    ///
    /// 线程创建失败没法在单测里人为制造，所以这里钉的是"上报"这一步：
    /// 只要走进这条上报，快照里就必须有一条能看懂、能指导下一步的错误。
    #[test]
    fn a_dead_session_thread_is_reported_to_the_ui() {
        let shared = Shared::new(egui::Context::default());
        report_session_start_failure(&shared, "操作系统拒绝创建线程");
        let snap = shared.snapshot();
        let msg = snap.error.expect("启动失败必须写进界面状态（不能只写日志）");
        assert!(msg.contains("操作系统拒绝创建线程"), "原因要带上：{msg}");
        assert!(msg.contains("快捷键"), "要说清楚后果（按键会没反应）：{msg}");
        assert!(msg.contains("重启"), "要给主人一条能走的路：{msg}");
        assert!(!snap.recording, "启动失败不能谎称在录音");
    }

    /// 全局键盘钩子没装上时，会话线程要把它写进界面状态。
    ///
    /// 回归（报告第 8 条）：钩子安装失败以前只写日志、程序"一切正常"，实际一个键
    /// 都没接管 —— 主人只会觉得"按快捷键没反应"。这条用例走的是真正的会话线程
    /// （端到端），只喂一条 `Cmd::StartupFailure`，不碰麦克风与网络。
    #[tokio::test]
    async fn a_hook_install_failure_reaches_the_ui_through_the_worker() {
        let shared = Arc::new(Shared::new(egui::Context::default()));
        let cfg = Arc::new(Mutex::new(Config::default()));
        let (tx, rx) = unbounded_channel();
        tx.send(Cmd::StartupFailure(
            "全局快捷键没能装上（SetWindowsHookExW 失败）：按快捷键不会有任何反应".into(),
        ))
        .unwrap();
        drop(tx); // 队列排空后 worker 自己退出
        worker(cfg, shared.clone(), rx).await;
        let msg = shared
            .snapshot()
            .error
            .expect("钩子装不上必须写进界面状态");
        assert!(msg.contains("没能装上"), "原因要原样带到界面上：{msg}");
    }
}
