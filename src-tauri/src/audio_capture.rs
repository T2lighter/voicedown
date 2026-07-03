/// VoiceDown - WASAPI 进程级 Loopback 音频捕获模块
///
/// 使用 wasapi crate (HEnquist/wasapi-rs v0.23) 的
/// `AudioClient::new_application_loopback_client()` 方法，
/// 实现按目标进程 PID 隔离捕获音频，仅捕获目标进程及其子进程树的音频输出。

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, atomic::AtomicU64};
use crossbeam_channel::Sender;
use wasapi::*;

use crate::dsp::{downsample_48k_to_16k, is_silent};
use crate::wav_render::render_wav;

// ── 错误类型 ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AudioCaptureError {
    ComInitFailed,
    ActivationFailed(String),
    InitFailed(String),
    StartFailed(String),
    CaptureError(String),
    WavWriteFailed(String),
}

impl std::fmt::Display for AudioCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComInitFailed => write!(f, "COM 初始化失败"),
            Self::ActivationFailed(e) => write!(f, "进程 Loopback 激活失败: {e}"),
            Self::InitFailed(e) => write!(f, "初始化音频客户端失败: {e}"),
            Self::StartFailed(e) => write!(f, "启动音频流失败: {e}"),
            Self::CaptureError(e) => write!(f, "捕获错误: {e}"),
            Self::WavWriteFailed(e) => write!(f, "WAV 文件写入失败: {e}"),
        }
    }
}

impl std::error::Error for AudioCaptureError {}

// ── 状态与配置 ────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum CaptureState {
    Idle,
    Capturing,
    Stopping,
    Error(String),
}

/// 捕获采样率（WASAPI Loopback 混音格式：48000Hz）
pub const CAPTURE_SAMPLE_RATE: u32 = 48000;

// ── 音频捕获器 ────────────────────────────────────────────

pub struct AudioCapturer {
    target_pid: u32,
    include_tree: bool,
    state: Arc<Mutex<CaptureState>>,
    accumulated: Arc<Mutex<Vec<f32>>>,
    /// 音频分块发送通道（发送给 ASR 引擎处理），None 表示不启用 ASR 联动
    audio_chunk_tx: Option<Sender<Vec<f32>>>,
    /// ISSUE-5：背压丢块计数（`try_send` 满时累加），供可观测性告警 + stop 统计。
    dropped_count: Arc<AtomicU64>,
}

impl AudioCapturer {
    /// 创建带 ASR 音频通道的捕获器（固定 300ms 分块模式）
    ///
    /// `audio_chunk_tx` 用于将捕获的音频分片（每约 300ms）发送给 ASR 引擎。
    /// Python 端 (asr_server.py) 用 StreamBuffer 攒满 600ms(9600 样本) 切一块喂
    /// ParaformerStreaming 流式识别，Rust 端无需关心语音段边界。
    ///
    /// `include_tree` 参数控制是否捕获目标进程及其子进程的音频。
    /// - true: 捕获整个进程树（窗口模式使用，可跨进程捕获 Chrome 渲染器）
    /// - false: 仅捕获目标进程自身（音频会话模式使用，实现单标签页隔离）
    pub fn new_with_asr(
        target_pid: u32,
        include_tree: bool,
        audio_chunk_tx: Sender<Vec<f32>>,
    ) -> Self {
        Self {
            target_pid,
            include_tree,
            state: Arc::new(Mutex::new(CaptureState::Idle)),
            accumulated: Arc::new(Mutex::new(Vec::new())),
            audio_chunk_tx: Some(audio_chunk_tx),
            dropped_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn get_state(&self) -> CaptureState {
        self.state.lock().unwrap().clone()
    }

    pub fn get_sample_count(&self) -> usize {
        self.accumulated.lock().unwrap().len()
    }

    pub fn get_sample_rate(&self) -> u32 {
        CAPTURE_SAMPLE_RATE
    }

    /// ISSUE-5：本次会话背压丢块总数。
    pub fn get_dropped_count(&self) -> u64 {
        self.dropped_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// ISSUE-5：丢块计数 handle（供 lib.rs ASR 主线程限流 emit `asr-warning`）。
    pub fn dropped_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped_count)
    }

    pub fn start_capturing(&self) -> Result<(), AudioCaptureError> {
        {
            let mut state = self.state.lock().unwrap();
            if *state == CaptureState::Capturing {
                return Ok(());
            }
            *state = CaptureState::Capturing;
        }

        let target_pid = self.target_pid;
        let include_tree = self.include_tree;
        let state = self.state.clone();
        let accumulated = self.accumulated.clone();
        let audio_chunk_tx = self.audio_chunk_tx.clone();
        let dropped_count = self.dropped_count.clone();

        std::thread::spawn(move || {
            let res = run_capture(
                target_pid,
                include_tree,
                &state,
                &accumulated,
                audio_chunk_tx,
                &dropped_count,
            );
            match res {
                Ok(()) => {
                    let mut s = state.lock().unwrap();
                    if !matches!(&*s, CaptureState::Error(_)) {
                        *s = CaptureState::Idle;
                    }
                }
                Err(e) => {
                    let mut s = state.lock().unwrap();
                    *s = CaptureState::Error(format!("{}", e));
                    eprintln!("[AudioCapturer] 捕获错误: {}", e);
                }
            }
        });

        Ok(())
    }

    pub fn stop_capturing(&self) {
        let mut state = self.state.lock().unwrap();
        if *state == CaptureState::Capturing {
            *state = CaptureState::Stopping;
        }
    }

    pub fn save_to_wav(&self, file_path: &str) -> Result<(), AudioCaptureError> {
        // 锁内直写（不 clone accumulated）：捕获线程已退出 + 导出期禁用开始 → 锁无竞争者。
        // 渲染逻辑在 wav_render::render_wav（纯函数，便于单测），本函数只做「空检查 + BufWriter 落盘」。
        let samples = self.accumulated.lock().unwrap();
        if samples.is_empty() {
            return Err(AudioCaptureError::WavWriteFailed("没有音频数据".into()));
        }
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(file_path)
                .map_err(|e| AudioCaptureError::WavWriteFailed(e.to_string()))?,
        );
        render_wav(&mut f, &samples, CAPTURE_SAMPLE_RATE)
            .map_err(|e| AudioCaptureError::WavWriteFailed(e.to_string()))?;
        f.flush()
            .map_err(|e| AudioCaptureError::WavWriteFailed(e.to_string()))?;
        let dur = samples.len() as f64 / CAPTURE_SAMPLE_RATE as f64;
        eprintln!("[AudioCapturer] WAV 已保存: {} ({:.1}s)", file_path, dur);
        Ok(())
    }
}

// ── 捕获主循环（wasapi crate 驱动）──────────────────────

/// ISSUE-5：向 ASR 通道发送一块音频，背压满时累加丢块计数（可观测性 REM-05）。
///
/// `try_send` 非阻塞：成功 → true；通道满（`Full`）→ `dropped` +1 返 false（音频块丢弃，
/// ASR 文本路径会缺这段，WAV 存档不受影响）；接收端已退（`Disconnected`，正常停止）→ 返 false
/// 但不计数（非背压丢块）。抽为独立纯逻辑便于单测（`run_capture` 依赖 wasapi 无法在测试中运行）。
fn deliver_chunk(
    tx: &crossbeam_channel::Sender<Vec<f32>>,
    chunk: Vec<f32>,
    dropped: &std::sync::atomic::AtomicU64,
) -> bool {
    use crossbeam_channel::TrySendError;
    match tx.try_send(chunk) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// ISSUE-7：静音块降频喂的间隔（每 N 个静音块喂 1 个保 ParaformerStreaming cache 上下文）。
/// 每 300ms 一块 → N=5 即每 1.5s 喂一次保 cache；语音块恢复时立即全喂（不丢首字）。
const SILENT_FEED_EVERY: u32 = 5;

/// ISSUE-7：静音 RMS 阈值默认值（env `SILENCE_RMS_THRESHOLD` 可覆盖，不同环境底噪不同）。
const SILENCE_RMS_THRESHOLD_DEFAULT: f32 = 0.01;

/// ISSUE-7：读取静音 RMS 阈值（env 覆盖优先，否则常量默认）。
fn silence_rms_threshold() -> f32 {
    std::env::var("SILENCE_RMS_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SILENCE_RMS_THRESHOLD_DEFAULT)
}

fn run_capture(
    pid: u32,
    include_tree: bool,
    state: &Arc<Mutex<CaptureState>>,
    accumulated: &Arc<Mutex<Vec<f32>>>,
    audio_chunk_tx: Option<Sender<Vec<f32>>>,
    dropped_count: &Arc<AtomicU64>,
) -> Result<(), AudioCaptureError> {
    // 1. COM 初始化（wasapi crate 内部调用 CoInitializeEx）
    if initialize_mta().is_err() {
        return Err(AudioCaptureError::ComInitFailed);
    }

    // 2. 创建进程级 Loopback 音频客户端
    let mut audio_client =
        AudioClient::new_application_loopback_client(pid, include_tree)
            .map_err(|e| AudioCaptureError::ActivationFailed(e.to_string()))?;

    // 3. 指定捕获格式：32-bit float, 48000Hz, 立体声
    let desired_format = WaveFormat::new(32, 32, &SampleType::Float, CAPTURE_SAMPLE_RATE as usize, 2, None);
    let block_align = desired_format.get_blockalign() as usize;

    // 4. 初始化客户端（共享模式 + 事件驱动）
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 0,
    };
    audio_client
        .initialize_client(&desired_format, &Direction::Capture, &mode)
        .map_err(|e| AudioCaptureError::InitFailed(e.to_string()))?;

    // 5. 获取事件句柄和捕获客户端
    let h_event = audio_client
        .set_get_eventhandle()
        .map_err(|e| AudioCaptureError::InitFailed(e.to_string()))?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|e| AudioCaptureError::InitFailed(e.to_string()))?;

    // 6. 启动音频流
    audio_client
        .start_stream()
        .map_err(|e| AudioCaptureError::StartFailed(e.to_string()))?;

    // 7. 捕获循环
    let mut sample_queue: VecDeque<u8> = VecDeque::new();
    let mut capture_error: Option<AudioCaptureError> = None;
    // ASR 分块缓冲区：每约 300ms 发送一次音频给 ASR 引擎（供流式 ASR）
    let chunk_samples = (CAPTURE_SAMPLE_RATE as f32 * 0.3) as usize; // 48000 * 0.3 = 14400 samples
    let mut asr_buffer: Vec<f32> = Vec::with_capacity(chunk_samples);
    // ISSUE-7：能量门控状态——静音块降频喂（每 SILENT_FEED_EVERY 块喂 1 个保 cache）
    let silence_threshold = silence_rms_threshold();
    let mut silent_streak: u32 = 0;

    loop {
        if *state.lock().unwrap() == CaptureState::Stopping {
            break;
        }

        // 读取可用帧数
        let new_frames = match capture_client.get_next_packet_size() {
            Ok(Some(n)) => n,
            Ok(None) => 0,
            Err(e) => {
                capture_error = Some(AudioCaptureError::CaptureError(e.to_string()));
                break;
            }
        };

        if new_frames > 0 {
            let additional = (new_frames as usize * block_align)
                .saturating_sub(sample_queue.capacity() - sample_queue.len());
            sample_queue.reserve(additional);

            capture_client
                .read_from_device_to_deque(&mut sample_queue)
                .map_err(|e| AudioCaptureError::CaptureError(e.to_string()))?;
        }

        // 从字节队列提取立体声 f32 → 单声道 f32，写入 accumulated
        while sample_queue.len() >= block_align {
            let left = f32::from_le_bytes([
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
            ]);
            let right = f32::from_le_bytes([
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
                sample_queue.pop_front().unwrap(),
            ]);
            let mono = (left + right) * 0.5;
            accumulated.lock().unwrap().push(mono);

            // 同时写入 ASR 分块缓冲区
            asr_buffer.push(mono);
        }

        // 每积累约 300ms 的音频，发送给 ASR 引擎（Python 端 StreamBuffer 攒块喂 ParaformerStreaming）
        if asr_buffer.len() >= chunk_samples {
            if let Some(ref tx) = audio_chunk_tx {
                let chunk = std::mem::replace(&mut asr_buffer, Vec::with_capacity(chunk_samples));
                // ISSUE-6：ASR 旁路下采样 48k→16k（WAV 存档仍用 accumulated 的 48k 原始，未动）。
                let chunk16 = downsample_48k_to_16k(&chunk);
                // ISSUE-7：能量门控——静音块降频喂（每 N 块 1 个保 cache 上下文，恢复说话不丢首字），
                // 语音块全喂。省去对纯静音的无意义 generate 前向（VAD-lite，不进流式 loop）。
                let feed = if is_silent(&chunk16, silence_threshold) {
                    silent_streak = silent_streak.saturating_add(1);
                    silent_streak % SILENT_FEED_EVERY == 0
                } else {
                    silent_streak = 0;
                    true
                };
                if feed {
                    // 非阻塞发送：ASR 处理不及时则丢旧数据避免堆积（背压丢块计入 dropped_count）
                    let _ = deliver_chunk(tx, chunk16, dropped_count);
                }
            }
        }

        // 等待事件或超时（100ms），超时后继续检查状态以便及时响应停止
        if h_event.wait_for_event(100).is_err() {
            if *state.lock().unwrap() == CaptureState::Stopping {
                break;
            }
        }
    }

    // 发送最后剩余不足 300ms 的音频（ISSUE-6：末尾尾巴也下采样到 16k 送 ASR）
    if !asr_buffer.is_empty() {
        if let Some(ref tx) = audio_chunk_tx {
            let tail = downsample_48k_to_16k(&std::mem::take(&mut asr_buffer));
            let _ = deliver_chunk(tx, tail, dropped_count);
        }
    }

    // 8. 停止流（无论正常结束还是出错都执行清理）
    let _ = audio_client.stop_stream();

    if let Some(err) = capture_error {
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use crossbeam_channel::bounded;

    #[test]
    fn deliver_chunk_empty_channel_sends() {
        // 空通道 → 成功发送，不计数
        let (tx, _rx) = bounded::<Vec<f32>>(1);
        let dropped = AtomicU64::new(0);
        assert!(deliver_chunk(&tx, vec![0.0], &dropped));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn deliver_chunk_full_counts_drop() {
        // 通道满 → 丢弃 + 计数递增
        let (tx, _rx) = bounded::<Vec<f32>>(1);
        let dropped = AtomicU64::new(0);
        let _ = tx.try_send(vec![0.0]); // 填满
        assert!(!deliver_chunk(&tx, vec![1.0], &dropped));
        assert!(!deliver_chunk(&tx, vec![2.0], &dropped));
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn deliver_chunk_disconnected_not_counted() {
        // 接收端已退（正常停止）→ 不计数（非背压丢块）
        let (tx, rx) = bounded::<Vec<f32>>(1);
        let dropped = AtomicU64::new(0);
        drop(rx);
        assert!(!deliver_chunk(&tx, vec![0.0], &dropped));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn save_to_wav_empty_returns_error_without_file() {
        // 空 accumulated → WavWriteFailed（policy 不变），且早返回不创建文件
        let (tx, _rx) = bounded::<Vec<f32>>(1);
        let cap = AudioCapturer::new_with_asr(0, true, tx);
        let path = "voicedown_render_wav_test_should_not_exist.wav";
        let err = cap.save_to_wav(path).unwrap_err();
        assert!(matches!(err, AudioCaptureError::WavWriteFailed(_)));
        assert!(!std::path::Path::new(path).exists());
    }
}
