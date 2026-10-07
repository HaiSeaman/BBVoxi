//! 麦克风采集：设备原生格式 → 单声道 f32 → 盒式滤波重采样到 16kHz → 100ms 帧。
//! 独立线程持有 cpal Stream（Stream 不是 Send，不能跨线程搬），帧通过 channel 送给会话任务。

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub const TARGET_RATE: u32 = 16_000;
/// 100ms 一帧（三家服务商都建议 100~200ms）
pub const FRAME_SAMPLES: usize = (TARGET_RATE as usize) / 10;

/// 下拉栏里的一台麦克风：`id` 存进配置、`label` 给人看。
///
/// 为什么显示的和存的要分开：主人认的是 Windows 里那个名字
/// （比如「麦克风 (BY-CM1)」），但**名字会重名** —— 两台同型号设备一模一样，
/// 只存名字就会出现"选的是它、开的却是另一台"。`id` 是 Windows 的端点编号
/// （`GetId()` 那条，形如 `wasapi:{0.0.1.00000000}.{...}`），唯一且重启、
/// 拔了再插都不变，所以拿它当身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicDevice {
    pub id: String,
    pub label: String,
}

/// 列出本机所有能录音的设备（设置窗下拉栏用）。
pub fn list_input_devices() -> Result<Vec<MicDevice>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for device in host.input_devices().context("枚举麦克风失败")? {
        // 编号读不出来就没法存进配置、下次也找不回来：这台不列，
        // 免得在下拉栏里摆一个"选了也不生效"的选项
        let Some(id) = guarded(|| device.id().ok().map(|i| i.to_string())) else {
            crate::log::log("有一台麦克风读不出设备编号，已从列表里跳过");
            continue;
        };
        out.push(MicDevice {
            id,
            label: device_label(&device),
        });
    }
    Ok(out)
}

/// 选定的那台麦克风现在还在不在（设置页用它决定要不要提醒主人）。
/// 空串 = 跟随系统默认，永远算「在」。
///
/// 判定与真正开流时**共用同一个 `pick`**：两处各写一份的话，日后改了一处就会
/// 出现"界面看着没问题、实际录的是另一台麦"这种最难查的毛病。
pub fn selected_is_missing(wanted: &str, mics: &[MicDevice]) -> bool {
    matches!(
        pick(wanted, mics.iter().map(|m| m.id.as_str())),
        Pick::Missing
    )
}

/// 录音时该开哪台麦克风（纯逻辑，方便单测钉住）。
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    /// 跟随系统默认设备（配置里没选过）
    Default,
    /// `ids` 里的第 n 台
    Index(usize),
    /// 选过，但那台现在不在（被拔掉 / 被停用）
    Missing,
}

/// `wanted` 空 = 跟随系统默认；否则按唯一编号在候选里找。
fn pick<'a>(wanted: &str, mut ids: impl Iterator<Item = &'a str>) -> Pick {
    if wanted.is_empty() {
        return Pick::Default;
    }
    match ids.position(|id| id == wanted) {
        Some(i) => Pick::Index(i),
        None => Pick::Missing,
    }
}

/// 下拉栏里显示给人看的名字。
///
/// 为什么不直接用 `description().name()`：WASAPI 那边它优先取的是**驱动描述**
/// （DeviceDesc），实测主人这台机器上只给出干巴巴的「麦克风」，两台设备会长得
/// 一模一样。真正好认的 FriendlyName（「麦克风 (BY-CM1)」）被 cpal 放在
/// `extended()` 里，所以优先用它，取不到再退回朴素名字。
fn mic_label(desc: &cpal::DeviceDescription) -> String {
    match desc.extended().first() {
        Some(friendly) if !friendly.trim().is_empty() && friendly != desc.name() => friendly.clone(),
        _ => desc.name().to_string(),
    }
}

/// 设备的显示名（只为写日志/界面，读不出来给个占位串，绝不因此让录音失败）
fn device_label(device: &cpal::Device) -> String {
    guarded(|| device.description().ok())
        .map(|d| mic_label(&d))
        .unwrap_or_else(|| "未知设备".to_string())
}

/// 读设备信息时**必须**兜住 panic。
///
/// 为什么：cpal 的 WASAPI 后端在 `description()` 里用了
/// `expect("could not open property store")` —— 设备恰好在这一瞬间被拔掉就会
/// panic。而这里的调用点在界面线程和采集线程上：一次 panic，轻则这次录音没了，
/// 重则整个托盘程序消失（主人只会看到"打开设置就闪退"）。宁可少列一台设备，
/// 也不能让程序死掉。读不出来返回 `None`，由调用方决定怎么呈现。
fn guarded<T>(read: impl FnOnce() -> Option<T>) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_or(None)
}

pub struct Capture {
    pub rx: Receiver<Vec<i16>>,
    /// 采集出错时的错误文本（采集线程写、会话读）
    error: Arc<Mutex<Option<String>>>,
    /// 请求采集线程收工（会话收尾时置位）
    stopped: Arc<AtomicBool>,
    /// 停止后"残留尾帧已经补发完了"（由采集回调置位，见 `Feed::push`）
    tail_flushed: Arc<AtomicBool>,
}

impl Capture {
    /// 采集过程中是否出过错（拔掉麦克风、设备被别的程序独占……）。
    /// 会话在 `rx` 结束（收到 `None`）时读一次：有错就如实报"麦克风异常"，
    /// 不要含糊地说"没有识别到内容"，否则主人会以为是自己的问题。
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 请求采集线程停下来并释放麦克风。
    /// 它比直接 `drop(capture)` 好：`rx` 仍留在调用方手里，缓冲里已录下的帧
    /// 还能读完，不会把尾巴（往往正是主人松手前的那几个字）一起丢掉。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// 残留尾帧补发完了吗（见 `Feed::push` 开头）。
    ///
    /// 会话收尾时**等这个标志**，而不是睡一个固定时长：蓝牙免提、部分 USB 声卡的
    /// 回调周期能到 100ms 以上，固定睡 60ms 会在回调到来之前就把流 drop 掉 ——
    /// 尾帧再也没机会补发，主人松手前那几个字就这么丢了。
    pub fn tail_flushed(&self) -> bool {
        self.tail_flushed.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    /// 丢掉采集句柄时**一定**叫采集线程收工。
    ///
    /// 为什么要有它：`stop()` 只是"请你停下来"，而采集线程真正退出还差一步 ——
    /// 它得等到回调再往通道里发一次满帧、撞上 Closed，或者回调自己报错。
    /// 会话里有一条提前 `return` 的路径（连 ASR 失败时），那条路上只把 Capture
    /// 丢掉、并没有调 `stop()`：常规设备 100~200ms 内就自愈了，但设备要是正好
    /// 不再产生回调（蓝牙/USB 声卡睡死、驱动卡住又不报错），采集线程就会一直
    /// 轮询、麦克风一直被占着（Windows 的"正在使用麦克风"一直亮）到进程结束。
    /// 这一行把"忘了 stop"这条路整个收掉。
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// 等麦克风就绪的上限。设备被独占、驱动卡死时 `build_input_stream` 可能一直不返回，
/// 而这里是**同步调用**（会话线程）—— 不设上限的话整个会话线程会一直卡着，
/// 之后所有热键（包括松开结束）都失效，只能重启程序。
const DEVICE_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 开始采集。`wanted` 是设置里选中的麦克风编号（空串 = 跟随系统默认设备）。
pub fn start(wanted: &str) -> Result<Capture> {
    // 100 帧 = 10 秒，够覆盖连接建立的耗时
    let (tx, rx) = channel::<Vec<i16>>(100);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    // stopped / error / tail_flushed 在这里创建、由采集线程和 Capture 共享：
    // 会话要能主动叫停采集（stop）、出错时读到原因（error）、收尾时知道尾巴补完没有。
    let stopped = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let tail_flushed = Arc::new(AtomicBool::new(false));
    let frame_dropped = Arc::new(AtomicBool::new(false));
    let tail_lost = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let thread_error = error.clone();
    let thread_tail = tail_flushed.clone();
    let thread_dropped = frame_dropped.clone();
    let thread_tail_lost = tail_lost.clone();
    // 采集线程要用这个值，先把借来的 &str 变成自己的一份
    let wanted = wanted.to_string();

    std::thread::Builder::new()
        .name("bbvoxi-audio".into())
        .spawn(move || {
            let handles = CaptureHandles {
                tx,
                stopped: thread_stopped.clone(),
                error: thread_error,
                tail_flushed: thread_tail.clone(),
                frame_dropped: thread_dropped.clone(),
                tail_lost: thread_tail_lost.clone(),
            };
            let stream = match build_stream(handles, &wanted) {
                Ok(s) => s,
                Err(e) => {
                    // `{e:#}` 而不是 `{e}`：anyhow 的 Display 只给最外层那句话
                    // （"打开麦克风输入流失败"），真正的原因（被独占、格式不支持、
                    // 设备被拔掉）全在因果链里，主人和排障都等着它。
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            // 停止条件有两个来源：会话主动 stop，或流自身出错时由回调置位。
            // 接收端被丢弃后回调也会置位 stopped，此时结束采集。
            // 循环里顺带把回调记下的两件"只能由这里来说"的事写进日志（见
            // `log_callback_notes`：回调里不能写日志）。
            while !thread_stopped.load(Ordering::Relaxed) {
                log_callback_notes(&thread_dropped, &thread_tail_lost);
                std::thread::sleep(Duration::from_millis(50));
            }
            // 会话可能还在等"尾帧补发完"：这里再给回调一点时间（**回调每来一批音频
            // 就查一次停止标志**），但**有上限**，绝不为了尾巴把收尾卡死。
            let deadline = std::time::Instant::now() + TAIL_WAIT_BEFORE_DROP;
            while !thread_tail.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            // 收工前把最后积压的话说完（否则"尾帧丢了"这条日志永远不会出现）
            log_callback_notes(&thread_dropped, &thread_tail_lost);
            drop(stream);
        })
        .context("启动音频线程失败")?;

    match ready_rx.recv_timeout(DEVICE_READY_TIMEOUT) {
        Ok(Ok(())) => Ok(Capture {
            rx,
            error,
            stopped,
            tail_flushed,
        }),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // 让采集线程别在后台继续挂着（它一旦就绪会立刻看到 stopped 并退出）
            stopped.store(true, Ordering::Relaxed);
            Err(anyhow!(
                "麦克风启动超时（{DEVICE_READY_TIMEOUT:?} 没就绪，通常是被别的程序独占或驱动卡住）"
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("音频线程异常退出")),
    }
}

/// 采集线程 drop 流之前，最多再等多久"尾帧补发完"。
/// 与 `Capture::tail_flushed` 是同一件事的两端（会话等的是同一个标志）。
const TAIL_WAIT_BEFORE_DROP: Duration = Duration::from_millis(300);

/// 采集回调需要的那几样共享句柄。
///
/// 收成一个结构体是为了别让它们各自成串地穿四层调用（`start` → `build_stream` →
/// `build` → `Feed`）：以前这四个参数每次都按同一个顺序传，插一个新参数就得改四处、
/// 还容易传错位置。打包之后只管往里加字段。
#[derive(Clone)]
struct CaptureHandles {
    tx: Sender<Vec<i16>>,
    stopped: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    tail_flushed: Arc<AtomicBool>,
    /// 缓冲满、丢过帧（**回调只置位**，日志由采集线程写，见 `log_callback_notes`）
    frame_dropped: Arc<AtomicBool>,
    /// 尾帧没能送出去（同上：回调只置位）
    tail_lost: Arc<AtomicBool>,
}

/// 把音频回调里记下的两件事写进日志 —— **在采集线程里写，绝不在回调里写**。
///
/// 为什么回调里连一行日志都不能写：音频回调跑在驱动的高优先级线程上，必须在一个
/// 缓冲区周期内返回；而 `log::log` 要抢一把全局锁、写文件、还要写 stderr，磁盘一忙
/// （杀软在扫、机械盘）就可能卡几十毫秒 —— 直接后果就是丢音，甚至被驱动判为卡顿。
/// 所以回调只置个标志（纳秒级），让每 50ms 醒一次的采集线程替它把话说完：
/// 日志一条不少，回调一个字节的额外活都不干。
///
/// 用 `swap(false)` 取走标志：同一个事件只写一次日志（`Feed` 那边也保证只置位一次），
/// 不会因为一直丢帧就把日志刷爆。
fn log_callback_notes(frame_dropped: &AtomicBool, tail_lost: &AtomicBool) {
    if frame_dropped.swap(false, Ordering::Relaxed) {
        crate::log::log("音频缓冲已满，丢弃了部分帧（连接建立较慢）");
    }
    if tail_lost.swap(false, Ordering::Relaxed) {
        // 两种原因都可能：缓冲满，或采集已被叫停（接收端退出了）。
        // 不硬说是哪一种 —— 猜错会把排障带到沟里。
        crate::log::log("尾帧没能送出去（缓冲已满或采集已停止）：最后一段音频可能丢了");
    }
}

fn build_stream(handles: CaptureHandles, wanted: &str) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, wanted)?;
    let label = device_label(&device);
    let supported = device.default_input_config().with_context(|| {
        format!("无法读取麦克风「{label}」的默认格式（可能被其他程序占用或未授权）")
    })?;

    let in_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();

    if in_rate == 0 || channels == 0 {
        bail!("麦克风参数异常：{in_rate}Hz / {channels}ch");
    }

    // 出错时只写日志是不够的：`stopped` 只在 channel 关闭时才置位，
    // 拔掉麦克风后界面会一直显示"录音中"、会话永远收不到结束信号。
    // 所以这里除了记日志，还要（1）把错误文本存到共享位置供会话读取，
    // （2）置位 stopped 让采集线程退出、rx 关闭 —— 会话随即正常收尾并如实报错。
    let on_err = {
        let stopped = handles.stopped.clone();
        let error = handles.error.clone();
        move |e: cpal::StreamError| {
            let msg = format!("{e}");
            crate::log::log(format!("麦克风采集错误：{msg}"));
            *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
            stopped.store(true, Ordering::Relaxed);
        }
    };
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, in_rate, channels, handles, on_err)?,
        SampleFormat::I16 => build::<i16>(&device, &config, in_rate, channels, handles, on_err)?,
        SampleFormat::U16 => build::<u16>(&device, &config, in_rate, channels, handles, on_err)?,
        SampleFormat::I32 => build::<i32>(&device, &config, in_rate, channels, handles, on_err)?,
        other => bail!("不支持的麦克风采样格式：{other:?}"),
    };
    stream.play().context("启动麦克风采集失败")?;
    // 日志里写清走的是哪条路：低于 16kHz 的输入（蓝牙免提）值得主人知道
    // 自己在用免提设备，识别质量本来就比立体声麦克风差一截。
    let how = match in_rate.cmp(&TARGET_RATE) {
        std::cmp::Ordering::Less => "线性插值升采样",
        std::cmp::Ordering::Equal => "原样通过",
        std::cmp::Ordering::Greater => "盒式滤波降采样",
    };
    crate::log::log(format!(
        "麦克风已启动：{label} {in_rate}Hz / {channels}ch / {format:?} → {how}到 {TARGET_RATE}Hz"
    ));
    Ok(stream)
}

/// 这次录音到底开哪台麦克风。
///
/// 两条兜底，都是为了"别让选麦克风这件小事把录音整个搞垮"：
/// 1. 枚举失败（驱动异常）也照常走系统默认设备 —— 不能因为列表读不出来就录不了音；
/// 2. 选中的那台现在不在（拔了 / 停用了）就回退系统默认，并留下日志 ——
///    主人下次打开设置会在那一行看到提醒，而不是对着"录不到声音"瞎猜。
fn pick_input_device(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let devices: Vec<cpal::Device> = match host.input_devices() {
        Ok(it) => it.collect(),
        Err(e) => {
            crate::log::log(format!("枚举麦克风失败（{e}），改用系统默认设备"));
            Vec::new()
        }
    };
    let ids: Vec<String> = devices
        .iter()
        .map(|d| guarded(|| d.id().ok().map(|i| i.to_string())).unwrap_or_default())
        .collect();
    let chosen = match pick(wanted, ids.iter().map(String::as_str)) {
        // 下标就是从这个列表里数出来的，理论上一定在；真取不到也只回退默认，不 panic
        Pick::Index(i) => devices.get(i).cloned(),
        Pick::Default => None,
        Pick::Missing => {
            // 三种情况都归到这里：设备被拔了、被停用了，或者列表根本没读出来。
            // 日志如实写成"没能用上"，别硬说是哪一种（猜错会把排障带到沟里）。
            crate::log::log(
                "设置里选的那台麦克风没能用上（已拔掉、被停用，或设备列表读不出来），\
                 本次改用系统默认设备；可在设置→通用里点「刷新」后重新选一台",
            );
            None
        }
    };
    match chosen {
        Some(d) => Ok(d),
        None => host.default_input_device().context("未检测到麦克风设备"),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    in_rate: u32,
    channels: usize,
    handles: CaptureHandles,
    on_err: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + ToF32 + Send + 'static,
{
    let mut feed = Feed {
        resampler: Resampler::new(in_rate, TARGET_RATE),
        frame: Vec::with_capacity(FRAME_SAMPLES),
        tx: handles.tx,
        stopped: handles.stopped,
        tail_flushed: handles.tail_flushed,
        frame_dropped: handles.frame_dropped,
        tail_lost: handles.tail_lost,
        channels,
        sum: 0.0,
        counted: 0,
        dropped: false,
        flushed: false,
    };
    device
        .build_input_stream(config, move |data: &[T], _| feed.push(data), on_err, None)
        .context("打开麦克风输入流失败")
}

struct Feed {
    resampler: Resampler,
    frame: Vec<i16>,
    tx: Sender<Vec<i16>>,
    stopped: Arc<AtomicBool>,
    /// 残留尾帧补发完的标志（会话收尾时等它，见 `Capture::tail_flushed`）
    tail_flushed: Arc<AtomicBool>,
    /// 丢过帧／尾帧没送出去（**回调只置位，日志由采集线程写**，见 `log_callback_notes`）
    frame_dropped: Arc<AtomicBool>,
    tail_lost: Arc<AtomicBool>,
    channels: usize,
    sum: f32,
    counted: usize,
    /// 是否已经因为缓冲满丢过帧（只置位一次，避免刷屏）
    dropped: bool,
    /// 停止后是否已经把残留尾帧补发过了（见 `push` 的开头，避免重复发）
    flushed: bool,
}

impl Feed {
    fn push<T: ToF32>(&mut self, data: &[T]) {
        // 停止录音后回调还可能再来最后一次：这时把不足一帧的残留样本、
        // 以及重采样器里还压着的样本一起补发出去。不补的话它们会随
        // `drop(stream)` 一起丢掉 —— 每次录音的尾巴会固定少最多 100ms 语音。
        // 不补零：ASR 接受任意长度的 PCM，真实的短帧比"后面接一段假静音"更准。
        if self.stopped.load(Ordering::Relaxed) {
            if !self.flushed {
                self.flushed = true;
                // 升采样时一次 push 可能攒了好几个输出样本，先全部吐出来
                while let Some(s) = self.resampler.flush() {
                    self.frame.push(s);
                }
                if !self.frame.is_empty() {
                    let tail = std::mem::take(&mut self.frame);
                    // 通道可能已经满了（网络慢时缓冲会攒到 100 帧）：这时这一帧
                    // ——往往正是主人松手前那几个字——会被丢掉。丢掉本身没法避免
                    // （回调里不能等），但**必须留下痕迹**：否则现象只是"最后一个
                    // 字少了"，日志里一条线索都没有。痕迹同样只置标志，由采集线程
                    // 落日志（见 `log_callback_notes`）。
                    if self.tx.try_send(tail).is_err() {
                        self.tail_lost.store(true, Ordering::Relaxed);
                    }
                }
                // 告诉会话"尾巴已经补完了"：它在收尾时等这个标志，而不是睡一个
                // 固定时长（固定时长在回调周期长的设备上会等不到，尾巴照样丢）。
                // 即使这次没有残留可补，也必须置位 —— "补完了"就是"以后没有了"。
                self.tail_flushed.store(true, Ordering::Relaxed);
            }
            return;
        }
        for s in data {
            self.sum += s.to_f32();
            self.counted += 1;
            if self.counted < self.channels {
                continue;
            }
            let mono = self.sum / self.channels as f32;
            self.sum = 0.0;
            self.counted = 0;
            self.resampler.push(mono);
            while let Some(sample) = self.resampler.pop() {
                self.frame.push(sample);
                if self.frame.len() >= FRAME_SAMPLES {
                    let full =
                        std::mem::replace(&mut self.frame, Vec::with_capacity(FRAME_SAMPLES));
                    use tokio::sync::mpsc::error::TrySendError;
                    match self.tx.try_send(full) {
                        Ok(()) => {}
                        // 还没连上 ASR 时缓冲会满：丢这一帧继续采，绝不能因此停掉麦克风。
                        // 只置标志，日志由采集线程写（回调里连一行日志都不能写，
                        // 见 `log_callback_notes`）
                        Err(TrySendError::Full(_)) => {
                            if !self.dropped {
                                self.dropped = true;
                                self.frame_dropped.store(true, Ordering::Relaxed);
                            }
                        }
                        // 接收端已退出，采集收工
                        Err(TrySendError::Closed(_)) => self.stopped.store(true, Ordering::Relaxed),
                    }
                }
            }
        }
    }
}

/// 采样率转换：把麦克风的原生采样率统一到 16kHz。
///
/// 为什么要分两条路走：`Decimator` 那套"分箱取均值"只能把**多个**输入样本
/// 合成一个输出样本。当输入比输出还少（蓝牙耳机免提模式常见 8kHz）时，
/// 它会退化成"一进一出"——输出速率还是 8kHz，却被当成 16kHz 送去识别，
/// 声音被慢放一倍，识别结果基本是乱码。升采样必须在样本之间插值。
pub enum Resampler {
    Down(Decimator),
    Up(Interpolator),
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        if in_rate >= out_rate {
            Resampler::Down(Decimator::new(in_rate, out_rate))
        } else {
            Resampler::Up(Interpolator::new(in_rate, out_rate))
        }
    }

    pub fn push(&mut self, sample: f32) {
        match self {
            Resampler::Down(d) => d.push(sample),
            Resampler::Up(u) => u.push(sample),
        }
    }

    pub fn pop(&mut self) -> Option<i16> {
        match self {
            Resampler::Down(d) => d.pop(),
            Resampler::Up(u) => u.pop(),
        }
    }

    /// 收尾时把还压在里面的样本吐出来（正常采样期间不要调用）。
    /// 降采样会有一个"还没闭合的箱"，升采样可能攒了几个待取的输出样本 ——
    /// 不吐出来它们就随重采样器一起丢掉，录音尾巴会少一截。
    pub fn flush(&mut self) -> Option<i16> {
        match self {
            Resampler::Down(d) => d.finish(),
            Resampler::Up(u) => u.pop(),
        }
    }
}

/// 线性插值升采样：输入的采样率低于目标时，在两个样本之间补出中间值。
///
/// 第 m 个输出样本落在输入时间轴的 `m * 输入率/输出率` 处，取它左右两个
/// 输入样本的线性插值。8k→16k 时正好是每两个样本之间补一个中点。
pub struct Interpolator {
    /// 每个输出样本在输入时间轴上推进的距离 = 输入率/输出率（< 1）
    step: f64,
    /// 下一个待计算的输出样本在输入时间轴上的位置
    pos: f64,
    /// 已经读入的输入样本个数
    idx: u64,
    /// 上一个输入样本（插值区间的左端点）
    prev: f64,
    /// 一次 push 可能产出多个输出样本（8k→16k 是两个），先攒着慢慢取
    ready: VecDeque<i16>,
}

impl Interpolator {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            pos: 0.0,
            idx: 0,
            prev: 0.0,
            ready: VecDeque::new(),
        }
    }

    pub fn push(&mut self, sample: f32) {
        let x = sample as f64;
        if self.idx == 0 {
            // 第一个样本既是时间轴 0 处的输出，也是后面插值的左端点
            self.prev = x;
            self.idx = 1;
            self.ready.push_back(to_i16(x));
            self.pos = self.step;
            return;
        }
        // 现在已知 [idx-1, idx] 这一段的两个端点，把落在这段里的输出点全算出来
        while self.pos <= self.idx as f64 {
            let left = (self.idx - 1) as f64;
            let frac = self.pos - left;
            self.ready
                .push_back(to_i16(self.prev + (x - self.prev) * frac));
            self.pos += self.step;
        }
        self.prev = x;
        self.idx += 1;
    }

    pub fn pop(&mut self) -> Option<i16> {
        self.ready.pop_front()
    }
}

/// 归一化浮点样本 → i16：先夹到 [-1, 1]（插值可能越界），再按 i16::MAX 缩放。
fn to_i16(sample: f64) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f64) as i16
}

/// 盒式滤波降采样：把输入样本按 (in_rate/out_rate) 分箱取均值。
/// 48k→16k 等价于三点平均；44.1k→16k 这类非整数比也能得到精确的输出速率。
/// 约定：调用方每次 push 后把 pop() 取空（最多只会积压一个待取样本）。
pub struct Decimator {
    step: f64,
    idx: u64,
    cur_bin: i64,
    sum: f64,
    count: u32,
    ready: Option<i16>,
}

impl Decimator {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            idx: 0,
            cur_bin: -1,
            sum: 0.0,
            count: 0,
            ready: None,
        }
    }

    pub fn push(&mut self, sample: f32) {
        let bin = (self.idx as f64 / self.step).floor() as i64;
        if bin != self.cur_bin {
            self.flush();
            self.cur_bin = bin;
        }
        self.sum += sample as f64;
        self.count += 1;
        self.idx += 1;
    }

    pub fn pop(&mut self) -> Option<i16> {
        self.ready.take()
    }

    /// 收尾专用：把当前正在积累的那个箱也闭合、送出去。
    /// 正常采样期间不能调用（它会把还没攒够的箱提前封口，破坏分箱边界）；
    /// 只在停止录音、要丢掉重采样器之前调一次，免得最后半个箱白白丢掉。
    pub fn finish(&mut self) -> Option<i16> {
        if self.ready.is_none() {
            self.flush();
        }
        self.ready.take()
    }

    fn flush(&mut self) {
        if self.count == 0 {
            return;
        }
        self.ready = Some(to_i16(self.sum / self.count as f64));
        self.sum = 0.0;
        self.count = 0;
    }
}

trait ToF32 {
    fn to_f32(&self) -> f32;
}

impl ToF32 for f32 {
    fn to_f32(&self) -> f32 {
        *self
    }
}

impl ToF32 for i16 {
    fn to_f32(&self) -> f32 {
        *self as f32 / 32768.0
    }
}

impl ToF32 for u16 {
    fn to_f32(&self) -> f32 {
        (*self as f32 - 32768.0) / 32768.0
    }
}

impl ToF32 for i32 {
    fn to_f32(&self) -> f32 {
        *self as f32 / 2147483648.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 下拉栏里显示给人看的名字，必须是 Windows「声音设置」里那个好认的名字。
    ///
    /// 为什么不直接用 `description().name()`：WASAPI 那边它优先取的是**驱动描述**
    /// （DeviceDesc），实测主人这台机器上只给出干巴巴的「麦克风」；两台不同型号
    /// 的设备会显示成一模一样，下拉栏里根本分不出谁是谁。真正好认的
    /// FriendlyName（「麦克风 (BY-CM1)」）被 cpal 放在 `extended()` 里。
    #[test]
    fn mic_label_prefers_the_windows_friendly_name() {
        let desc = cpal::DeviceDescriptionBuilder::new("麦克风")
            .add_extended_line("麦克风 (BY-CM1)")
            .build();
        assert_eq!(mic_label(&desc), "麦克风 (BY-CM1)");
    }

    /// 没有更详细的附加行时，退回朴素名字 —— 不能显示成空白，否则下拉栏
    /// 里会出现一行点不中、也认不出的空选项。
    #[test]
    fn mic_label_falls_back_to_the_plain_name() {
        let plain = cpal::DeviceDescriptionBuilder::new("麦克风").build();
        assert_eq!(mic_label(&plain), "麦克风");

        // 附加行与名字重复时别显示成「麦克风（麦克风）」
        let same = cpal::DeviceDescriptionBuilder::new("麦克风")
            .add_extended_line("麦克风")
            .build();
        assert_eq!(mic_label(&same), "麦克风");

        // 空白附加行同样不能用
        let blank = cpal::DeviceDescriptionBuilder::new("麦克风")
            .add_extended_line("   ")
            .build();
        assert_eq!(mic_label(&blank), "麦克风");
    }

    /// 没选（配置里是空串）= 跟随系统默认设备。老配置文件里没有这一项，
    /// 读出来就是空串 —— 行为必须和升级前一模一样。
    #[test]
    fn empty_mic_selection_uses_the_system_default() {
        assert_eq!(pick("", std::iter::empty()), Pick::Default);
        assert_eq!(pick("", ["wasapi:a"].into_iter()), Pick::Default);
    }

    /// 选过设备：按唯一编号在下拉栏的候选里找到它（名字会重名，编号不会）。
    #[test]
    fn saved_mic_is_found_by_its_device_id() {
        let ids = ["wasapi:a", "wasapi:b", "wasapi:c"];
        assert_eq!(pick("wasapi:b", ids.iter().copied()), Pick::Index(1));
        assert_eq!(
            pick("wasapi:b", ids.iter().copied()),
            Pick::Index(1),
            "顺序必须跟候选列表一致，否则会开到别的麦克风"
        );
    }

    /// 回归（选定的麦克风被拔掉 / 被禁用）：必须如实报"不在"，
    /// 由调用方回退系统默认并留下痕迹 —— 不能静默换成列表里的第一台，
    /// 否则主人会被从完全不相干的设备录音，还找不到原因。
    #[test]
    fn unplugged_mic_is_reported_as_missing() {
        let ids = ["wasapi:a", "wasapi:b"];
        assert_eq!(
            pick("wasapi:gone", ids.iter().copied()),
            Pick::Missing,
            "选中的设备不在了，必须报 Missing 而不是随便挑一台"
        );
    }

    /// 设置页那行提醒用的是**同一套判定**（`selected_is_missing`）：
    /// 两处各写一份的话，日后改了一处就会出现"界面说没问题、实际录的是别的麦"。
    #[test]
    fn missing_mic_check_agrees_with_the_capture_decision() {
        let mics = vec![
            MicDevice {
                id: "wasapi:a".into(),
                label: "麦克风 (BY-CM1)".into(),
            },
            MicDevice {
                id: "wasapi:b".into(),
                label: "耳机麦克风".into(),
            },
        ];
        assert!(
            !selected_is_missing("", &mics),
            "跟随系统默认永远算「在」，不该提醒主人"
        );
        assert!(!selected_is_missing("wasapi:b", &mics));
        assert!(
            selected_is_missing("wasapi:gone", &mics),
            "选中的设备不在了，设置页必须提醒"
        );
    }

    fn drain(dec: &mut Decimator, input: &[f32]) -> Vec<i16> {
        let mut out = Vec::new();
        for s in input {
            dec.push(*s);
            if let Some(v) = dec.pop() {
                out.push(v);
            }
        }
        out
    }

    /// 喂完输入、收干输出（升采样时一次 push 可能吐出多个样本，所以要收干净）
    fn run(r: &mut Resampler, input: &[f32]) -> Vec<i16> {
        let mut out = Vec::new();
        for s in input {
            r.push(*s);
            while let Some(v) = r.pop() {
                out.push(v);
            }
        }
        out
    }

    /// 最后一个箱要等下一个样本进来才闭合，所以末尾多喂 1 个样本再断言
    #[test]
    fn downsample_48k_to_16k_keeps_rate_and_level() {
        let mut dec = Decimator::new(48_000, 16_000);
        let mut input = vec![0.5f32; 48_000];
        input.push(0.0);
        let out = drain(&mut dec, &input);
        assert_eq!(out.len(), 16_000);
        // 恒定信号的平均值应保持不变（0.5 * 32767 ≈ 16383）
        assert!(out.iter().all(|v| (*v - 16383).abs() <= 1), "电平被改变");
    }

    #[test]
    fn downsample_44100_to_16k_rate_is_exact() {
        let mut dec = Decimator::new(44_100, 16_000);
        let mut input = vec![0.25f32; 44_100];
        input.push(0.0);
        let out = drain(&mut dec, &input);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn same_rate_passes_samples_through() {
        let mut dec = Decimator::new(16_000, 16_000);
        let out = drain(&mut dec, &[0.0, 1.0, -1.0, 0.5, 0.0]);
        assert_eq!(out, vec![0, 32767, -32767, 16383]);
    }

    #[test]
    fn frame_size_is_100ms() {
        assert_eq!(FRAME_SAMPLES, 1600);
    }

    /// 回归（低于 16kHz 的麦克风不会被升采样）：
    /// 蓝牙耳机免提模式常见 8kHz 输入，服务端只认 16kHz —— 不补采样就等于
    /// 把声音慢放一倍送过去，识别结果基本是乱码。输出速率必须是输入的 2 倍。
    #[test]
    fn upsamples_low_rate_input_to_target_rate() {
        let mut r = Resampler::new(8_000, TARGET_RATE);
        let input = vec![0.5f32; 80_000];
        let out = run(&mut r, &input);
        let ratio = out.len() as f64 / input.len() as f64;
        assert!(
            (ratio - 2.0).abs() < 0.001,
            "8kHz 输入必须按 2 倍升采样到 16kHz，实际速率比只有 {ratio:.4}"
        );
    }

    /// 升采样必须在两个样本之间**插值**，不能是"每个样本原样重复两遍"
    /// （重复=零阶保持，会在频谱里产生镜像，听感发毛，识别也更差）。
    /// 期望值按线性插值手算：位置 0/0.5/1/1.5/2/2.5/3 处的 [-1,-0.75,-0.5,-0.25,0,0.25,0.5]。
    #[test]
    fn upsampling_interpolates_between_neighbours() {
        let mut r = Resampler::new(8_000, 16_000);
        let out = run(&mut r, &[-1.0f32, -0.5, 0.0, 0.5]);
        assert_eq!(out, vec![-32767, -24575, -16383, -8191, 0, 8191, 16383]);
    }

    /// 16kHz 及以上的输入不能被这次改动影响（走原来的降采样路径）
    #[test]
    fn decimation_path_is_unchanged() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut input = vec![0.5f32; 48_000];
        input.push(0.0);
        let out = run(&mut r, &input);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|v| (*v - 16383).abs() <= 1), "电平被改变");
    }

    /// 回归（每次录音尾巴固定少最多 100ms）：停止录音时，回调里那半帧
    /// （不足一帧的残留）以前会随 `drop(stream)` 一起丢掉，主人最后那几个字
    /// 常常听不完整。这里验证 `Feed` 在 stopped 置位后会把残留尾帧补发出去。
    ///
    /// 构造方式不走真麦克风、也不注入：直接搭一个 `Feed` + 内存 channel，
    /// 先正常采一小段（不足一帧），再置位 stopped，然后模拟"最后一次回调"。
    #[test]
    fn stopping_flushes_the_partial_tail_instead_of_dropping_it() {
        let (tx, mut rx) = channel::<Vec<i16>>(10);
        let stopped = Arc::new(AtomicBool::new(false));
        let tail_flushed = Arc::new(AtomicBool::new(false));
        let mut feed = Feed {
            resampler: Resampler::new(TARGET_RATE, TARGET_RATE),
            frame: Vec::new(),
            tx,
            stopped: stopped.clone(),
            tail_flushed: tail_flushed.clone(),
            frame_dropped: Arc::new(AtomicBool::new(false)),
            tail_lost: Arc::new(AtomicBool::new(false)),
            channels: 1,
            sum: 0.0,
            counted: 0,
            dropped: false,
            flushed: false,
        };
        assert!(
            !tail_flushed.load(Ordering::Relaxed),
            "还没停止就不该声称尾巴补完了"
        );

        // 正常采一小段，还凑不满一整帧
        feed.push(&[0.1f32; 100]);
        assert!(rx.try_recv().is_err(), "不足一帧时不该往外发，应该先攒着");

        // 停止录音：下一次回调要把残留尾帧补发出去，而不是丢掉
        stopped.store(true, Ordering::Relaxed);
        feed.push(&[0.1f32; 10]);

        let tail = rx
            .try_recv()
            .expect("停止后应把残留尾帧补发出去，而不是随 drop(stream) 丢掉");
        assert!(
            tail.len() >= 100,
            "补发的尾帧要包含停止前攒下的样本，实际只有 {} 个",
            tail.len()
        );
        // 会话收尾就是等这个标志（而不是睡固定时长）：
        // 不置位的话它会一直等到超时上限，尾巴就有被丢的风险
        assert!(
            tail_flushed.load(Ordering::Relaxed),
            "补发完必须置位 tail_flushed，会话才知道可以收尾了"
        );

        // 只补发一次：再来回调也不该重复发
        feed.push(&[0.1f32; 10]);
        assert!(rx.try_recv().is_err(), "残留尾帧只能补发一次，不能重复");
    }

    /// 停止时哪怕**没有**残留可补（正好整齐）也要置位 `tail_flushed` ——
    /// 会话等的是"补完了"，不是"补过东西了"；不置位它会一直等到超时上限。
    #[test]
    fn stopping_without_leftovers_still_marks_the_tail_as_done() {
        let (tx, _rx) = channel::<Vec<i16>>(10);
        let stopped = Arc::new(AtomicBool::new(false));
        let tail_flushed = Arc::new(AtomicBool::new(false));
        let mut feed = Feed {
            resampler: Resampler::new(TARGET_RATE, TARGET_RATE),
            frame: Vec::new(),
            tx,
            stopped: stopped.clone(),
            tail_flushed: tail_flushed.clone(),
            frame_dropped: Arc::new(AtomicBool::new(false)),
            tail_lost: Arc::new(AtomicBool::new(false)),
            channels: 1,
            sum: 0.0,
            counted: 0,
            dropped: false,
            flushed: false,
        };

        stopped.store(true, Ordering::Relaxed);
        feed.push(&[] as &[f32]);
        assert!(
            tail_flushed.load(Ordering::Relaxed),
            "没有残留也要置位，否则会话会白等到超时"
        );
    }

    /// 回归（音频回调里做文件 I/O）：通道满或接收端已退出时，尾帧发不出去 ——
    /// 回调**只许置标志**，日志交给采集线程写（回调卡住就是丢音）。
    /// 这条测试盯的是"标志确实被置起来了"，也就是那条日志还会出现。
    #[test]
    fn a_tail_that_cannot_be_sent_raises_the_flag_instead_of_logging() {
        let (tx, rx) = channel::<Vec<i16>>(1);
        let tail_lost = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut feed = Feed {
            resampler: Resampler::new(TARGET_RATE, TARGET_RATE),
            frame: Vec::new(),
            tx,
            stopped: stopped.clone(),
            tail_flushed: Arc::new(AtomicBool::new(false)),
            frame_dropped: Arc::new(AtomicBool::new(false)),
            tail_lost: tail_lost.clone(),
            channels: 1,
            sum: 0.0,
            counted: 0,
            dropped: false,
            flushed: false,
        };

        // 先正常采一小段（不足一帧，攒在 frame 里），再把接收端丢掉、置位停止：
        // 下一次回调要补发尾帧，而这时必然发不出去
        feed.push(&[0.1f32; 100]);
        drop(rx);
        stopped.store(true, Ordering::Relaxed);
        feed.push(&[0.1f32; 10]);

        assert!(
            tail_lost.load(Ordering::Relaxed),
            "尾帧发不出去必须置标志，否则这条线索永远不会进日志"
        );
        // 取走标志（采集线程就是这么做的），不能重复报同一条
        log_callback_notes(&Arc::new(AtomicBool::new(false)), &tail_lost);
        assert!(
            !tail_lost.load(Ordering::Relaxed),
            "说过一次之后要清掉，免得一直丢帧就把日志刷爆"
        );
    }

    /// 真机验证（默认不跑，手动跑：`cargo test -- --ignored --nocapture`）：
    /// 1. 列出本机的麦克风，确认名字和编号都读得出来；
    /// 2. 按「设置里选中的那台」真的开一次麦克风，确认录得到音频帧；
    /// 3. 选一台**不存在**的设备，确认会退回系统默认（而不是报错、也不是录不到）。
    ///
    /// 为什么要它：上面那些纯函数测试只能证明"该选谁"的判断对，证明不了 WASAPI
    /// 那边**真的开到了那台设备**。这件事只有真开一次麦克风才算验证过。
    /// 为什么默认不跑：它动真麦克风（本机没麦 / 麦克风被禁用就会失败），
    /// 常规 `cargo test` 不该被硬件状态拖累。
    #[test]
    #[ignore = "需要真实麦克风；手动跑：cargo test -- --ignored --nocapture"]
    fn real_microphone_is_listed_selected_and_captured() {
        let mics = list_input_devices().expect("枚举本机麦克风");
        println!("本机麦克风 {} 台：", mics.len());
        for m in &mics {
            println!("  - {}  [{}]", m.label, m.id);
        }
        assert!(!mics.is_empty(), "本机没有可用的麦克风，这条测试验证不了");

        // 故意选列表里的最后一台（通常不是系统默认那台），验证"选的能被真的开起来"
        let target = mics.last().unwrap();
        let frames = capture_for_a_moment(&target.id);
        println!("选中「{}」录到 {frames} 帧", target.label);
        assert!(frames > 0, "选中了「{}」却一帧都没录到", target.label);

        // 光看"能录到"是不够的：本机只有一台麦克风时，"按编号选中"和"回退默认"
        // 结果一模一样，证明不了匹配逻辑对。这里直接核对**解析出来的设备编号**
        // ——它必须就是选的那一台，而不是"随便开了一个能用的"。
        let host = cpal::default_host();
        let resolved = pick_input_device(&host, &target.id).expect("按编号解析麦克风");
        assert_eq!(
            guarded(|| resolved.id().ok().map(|i| i.to_string())).as_deref(),
            Some(target.id.as_str()),
            "按编号选出来的不是这一台设备"
        );

        // 选一台不存在的设备：必须退回系统默认并照常录到声音（不能直接失败）
        let ghost = "wasapi:{0.0.0.00000000}.{00000000-0000-0000-0000-000000000000}";
        assert!(selected_is_missing(ghost, &mics), "这台设备本来就不该存在");
        let frames = capture_for_a_moment(ghost);
        println!("选了一台不存在的设备，退回系统默认后录到 {frames} 帧");
        assert!(
            frames > 0,
            "选中的设备不在时应该退回系统默认设备，而不是录不到声音"
        );
    }

    /// 开一次麦克风、录约 0.6 秒，返回收到的帧数（真机验证用）
    fn capture_for_a_moment(wanted: &str) -> usize {
        let mut capture = start(wanted).expect("打开麦克风");
        let deadline = std::time::Instant::now() + Duration::from_millis(600);
        let mut frames = 0;
        while std::time::Instant::now() < deadline {
            if capture.rx.try_recv().is_ok() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        capture.stop();
        // 给采集线程一点时间真的把设备放掉，免得连开两次时互相打架
        std::thread::sleep(Duration::from_millis(150));
        frames
    }

    /// `Decimator::finish` 要把当前正在积累的箱也收掉，
    /// 否则最后半个箱会随重采样器一起丢掉（哪怕只有 1~2 个样本也不该丢）。
    #[test]
    fn decimator_finish_closes_the_last_unfinished_bin() {
        let mut dec = Decimator::new(16_000, 16_000);
        // 先喂两个样本：第一个样本的箱会在第二个样本进来时闭合并被取走
        dec.push(0.5);
        assert_eq!(dec.pop(), None);
        dec.push(0.5);
        assert_eq!(dec.pop(), Some(16383));
        // 现在第二个样本还在箱里没闭合，finish 应把它收出来
        assert_eq!(
            dec.finish(),
            Some(16383),
            "收尾要把没闭合的最后一个箱吐出来"
        );
        assert_eq!(dec.pop(), None, "收干净之后不应再冒出来样本");
    }
}
