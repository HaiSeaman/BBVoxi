//! 麦克风采集：设备原生格式 → 单声道 f32 → 盒式滤波重采样到 16kHz → 100ms 帧。
//! 独立线程持有 cpal Stream（Stream 不是 Send，不能跨线程搬），帧通过 channel 送给会话任务。

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub const TARGET_RATE: u32 = 16_000;
/// 100ms 一帧（三家服务商都建议 100~200ms）
pub const FRAME_SAMPLES: usize = (TARGET_RATE as usize) / 10;

pub struct Capture {
    pub rx: Receiver<Vec<i16>>,
}

pub fn start() -> Result<Capture> {
    // 100 帧 = 10 秒，够覆盖连接建立的耗时
    let (tx, rx) = channel::<Vec<i16>>(100);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("bbvoxi-audio".into())
        .spawn(move || {
            let stopped = Arc::new(AtomicBool::new(false));
            let stream = match build_stream(tx, stopped.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            // 接收端被丢弃后回调会置位 stopped，此时结束采集
            while !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(stream);
        })
        .context("启动音频线程失败")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Capture { rx }),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => Err(anyhow!("音频线程异常退出")),
    }
}

fn build_stream(tx: Sender<Vec<i16>>, stopped: Arc<AtomicBool>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_input_device().context("未检测到麦克风设备")?;
    let supported = device
        .default_input_config()
        .context("无法读取麦克风默认格式（可能被其他程序占用或未授权）")?;

    let in_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();

    if in_rate == 0 || channels == 0 {
        bail!("麦克风参数异常：{in_rate}Hz / {channels}ch");
    }

    let on_err = |e| crate::log::log(format!("麦克风采集错误：{e}"));
    let stream = match format {
        SampleFormat::F32 => {
            build::<f32>(&device, &config, in_rate, channels, tx, stopped, on_err)?
        }
        SampleFormat::I16 => {
            build::<i16>(&device, &config, in_rate, channels, tx, stopped, on_err)?
        }
        SampleFormat::U16 => {
            build::<u16>(&device, &config, in_rate, channels, tx, stopped, on_err)?
        }
        SampleFormat::I32 => {
            build::<i32>(&device, &config, in_rate, channels, tx, stopped, on_err)?
        }
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
        "麦克风已启动：{in_rate}Hz / {channels}ch / {format:?} → {how}到 {TARGET_RATE}Hz"
    ));
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    in_rate: u32,
    channels: usize,
    tx: Sender<Vec<i16>>,
    stopped: Arc<AtomicBool>,
    on_err: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + ToF32 + Send + 'static,
{
    let mut feed = Feed {
        resampler: Resampler::new(in_rate, TARGET_RATE),
        frame: Vec::with_capacity(FRAME_SAMPLES),
        tx,
        stopped,
        channels,
        sum: 0.0,
        counted: 0,
        dropped: false,
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
    channels: usize,
    sum: f32,
    counted: usize,
    /// 是否已经因为缓冲满丢过帧（只记一次日志，避免刷屏）
    dropped: bool,
}

impl Feed {
    fn push<T: ToF32>(&mut self, data: &[T]) {
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
                        // 还没连上 ASR 时缓冲会满：丢这一帧继续采，绝不能因此停掉麦克风
                        Err(TrySendError::Full(_)) => {
                            if !self.dropped {
                                self.dropped = true;
                                crate::log::log("音频缓冲已满，丢弃了部分帧（连接建立较慢）");
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
}
