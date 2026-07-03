//! VoiceDown - ASR 会话三线程编排（deep module）
//!
//! 收敛 ASR 主线程（feed 子线程 + 主线程双线程解耦 + flush 收尾）、ct-punc 标点线程、
//! F-14 优化线程的 spawn 编排。interface 是各 `spawn_*` 函数（`start_capture` 调用），
//! 实现藏线程时序红线（双线程解耦 / flush / drain / emit_json）。A3 候选自 lib.rs 迁出
//!（a 保守搬迁：行为零改，红线逐条保，不补测靠 e2e）。
//!
//! A3-a1：`flush_optimizer` + `spawn_optimizer_thread`（F-14 优化旁路）。
//! A3-a2：`spawn_punc_thread`（ct-punc）。A3-a3：`spawn_asr_thread` + `should_warn`。

use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender};
use tauri::{AppHandle, Emitter};

use crate::dictionary;
use crate::python_bridge;
use crate::punc_pipeline;
use crate::text_optimizer;
use crate::text_postprocess::t2s;
use crate::{OptimizeEvent, TranscriptionEvent};

/// 优化批量 flush：drain 攒批句子 → backend.optimize（overlap 重写）→ 写回 optimized_text + emit。
/// 失败降级（out 空 → apply_output 走 fallback 原文），永不丢原文。
pub(crate) fn flush_optimizer(
    buf: &mut text_optimizer::OptimizerBuffer,
    rewriter: &mut text_optimizer::OverlapRewriter,
    backend: &dyn text_optimizer::LlmBackend,
    optimized_text: &Arc<Mutex<String>>,
    app_handle: &AppHandle,
) {
    let raw = buf.drain_new_sentences();
    if raw.trim().is_empty() {
        return;
    }
    let input = rewriter.build_input(&raw);
    let out = backend.optimize(&input, "optimize").unwrap_or_else(|e| {
        let _ = app_handle.emit("asr-optimize-warn", e);
        String::new() // 降级：空串触发 apply_output fallback
    });
    rewriter.apply_output(&out, &raw);
    let full = rewriter.full_text();
    // 同步 optimized_text（供停止后 finalize_document 读取）
    *optimized_text.lock().unwrap() = full.clone();
    let _ = app_handle.emit(
        "asr-optimize",
        OptimizeEvent {
            optimized: String::new(),
            full_optimized: full,
            is_final: false,
        },
    );
}

/// 优化线程：收句子 → 攒批 → flush（overlap 重写）；stop 时 drain+flush+finalize 后退出。
pub(crate) fn spawn_optimizer_thread(
    app_handle: AppHandle,
    sentence_rx: Receiver<String>,
    optimized_text: Arc<Mutex<String>>,
    running: Arc<Mutex<bool>>,
    backend: Box<dyn text_optimizer::LlmBackend>,
) {
    std::thread::spawn(move || {
        let mut buf = text_optimizer::OptimizerBuffer::new();
        let mut rewriter = text_optimizer::OverlapRewriter::new();
        loop {
            // 停止信号：drain 剩余句子 + flush 尾段后退出
            if !*running.lock().unwrap() {
                while let Ok(s) = sentence_rx.try_recv() {
                    buf.push(s);
                }
                if !buf.is_empty() {
                    flush_optimizer(&mut buf, &mut rewriter, &*backend, &optimized_text, &app_handle);
                }
                break;
            }
            match sentence_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(s) => {
                    if buf.would_exceed(&s) {
                        // 加 s 会超 100 字 → 先发掉已累积的（不含 s），s 另起一批
                        flush_optimizer(&mut buf, &mut rewriter, &*backend, &optimized_text, &app_handle);
                    }
                    buf.push(s);
                    if buf.should_flush_size() {
                        flush_optimizer(&mut buf, &mut rewriter, &*backend, &optimized_text, &app_handle);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if buf.should_flush_time() {
                        flush_optimizer(&mut buf, &mut rewriter, &*backend, &optimized_text, &app_handle);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // sentence_tx 已断开（ASR 线程退出）：drain 残留 + flush 尾段，避免丢失
                    while let Ok(s) = sentence_rx.try_recv() {
                        buf.push(s);
                    }
                    if !buf.is_empty() {
                        flush_optimizer(&mut buf, &mut rewriter, &*backend, &optimized_text, &app_handle);
                    }
                    break;
                }
            }
        }

        // 收尾：overlap 吸收进 committed，写回 optimized_text，emit is_final
        rewriter.finalize();
        let full = rewriter.full_text();
        *optimized_text.lock().unwrap() = full.clone();
        let _ = app_handle.emit(
            "asr-optimize",
            OptimizeEvent {
                optimized: String::new(),
                full_optimized: full.clone(),
                is_final: true,
            },
        );
        *running.lock().unwrap() = false;
        eprintln!("[Optimizer Thread] 退出，优化文本长度: {}", full.len());
    });
}

/// ISSUE-8：ct-punc 标点线程（懒加载：始终起，按 `bridge.punc_ready` 决定路径）。
///
/// 线程壳只跑 IO 时序（`recv_timeout` / `emit` / `request_punc`+`recv`+降级），业务决策
/// 「句→批→标点→降级」在 [`punc_pipeline::PuncPipeline`] 状态机（C 候选，全可单测）：
///
/// - 就绪（punc_ready=true）：`pipeline.push` 累积原文 → `decide_flush` 达阈值或 `finalize`
///   收尾时产出 `RequestPunc{raw_full}` → 壳整体重跑 ct-punc（跨 chunk 句界标点正确）
///   → 整体覆盖 transcription_text + emit。
/// - 未就绪/降级（punc_ready=false，ct-punc 后台加载中或失败）：`pipeline.push` 产出
///   `PassThrough` → 壳逐句直写主字幕（复刻离线降级实时性，不退化成批量 flush）。
///   raw_full 始终累积，就绪后首次 flush 整体标点全文（含逐句直写过部分）。
///
/// 独立线程，不阻塞流式 generate（红线）。降级铁律：punc 失败/超时/未就绪 → 原文回退，永不丢字。
/// 退出前 drain punc_rx（防跨会话脏消息错配）。
pub(crate) fn spawn_punc_thread(
    app_handle: AppHandle,
    bridge: Arc<python_bridge::PythonAsrBridge>,
    sentence_rx: Receiver<String>,
    transcription_text: Arc<Mutex<String>>,
    running: Arc<Mutex<bool>>,
) {
    std::thread::Builder::new()
        .name("asr-punc".into())
        .spawn(move || {
            // 起始 drain punc_rx：清掉上一会话可能的滞留响应（会话 A flush 超时降级后
            // Python 响应才到，退出时 drain 漏在途消息；会话 B 起始再 drain 兜底，防首帧
            // 误取上会话响应致短暂错字）。与退出时 drain 双重保险。
            while bridge.punc_rx.try_recv().is_ok() {}
            let mut pipeline = punc_pipeline::PuncPipeline::new();
            let mut last_flush = std::time::Instant::now();

            // ── IO 辅助：执行 PuncAction（决策在 pipeline，IO 在壳）──

            // PassThrough：未就绪/降级句直写主字幕（emit text=s, full=transcription_text 累积, is_final=false）。
            // 复刻 punc_tx=None 离线降级实时性——不退化成累积到阈值才批量上屏。
            let passthrough_io = |s: &str| {
                let full = {
                    let mut t = transcription_text.lock().unwrap();
                    t.push_str(s);
                    t.clone()
                };
                let _ = app_handle.emit(
                    "asr-transcription",
                    TranscriptionEvent {
                        text: s.to_string(),
                        full_text: full,
                        language: "auto".to_string(),
                        is_final: false,
                    },
                );
            };

            // RequestPunc：就绪路径整体重跑 ct-punc → 覆盖 transcription_text + emit。
            // 降级铁律：写 stdin / recv 超时 / 业务 error / 空 → 原文降级，永不丢字、永不阻塞。
            let request_punc_io = |text: &str| {
                if text.is_empty() {
                    return;
                }
                // 未就绪双保险（pipeline 已按 ready 决策，此处原子再读防就绪态竞态翻转）。
                if !bridge.punc_ready.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let punctuated = match bridge.request_punc(text) {
                    Ok(()) => match bridge.punc_rx.recv_timeout(std::time::Duration::from_secs(15)) {
                        Ok(r) => match &r.error {
                            None if !r.text.is_empty() => r.text,
                            _ => text.to_string(),
                        },
                        Err(_) => text.to_string(), // 超时 → 原文降级
                    },
                    Err(_) => text.to_string(), // 写 stdin 失败（Python 可能已崩溃）→ 原文降级
                };
                let full = {
                    let mut t = transcription_text.lock().unwrap();
                    *t = punctuated;
                    t.clone()
                };
                let _ = app_handle.emit(
                    "asr-transcription",
                    TranscriptionEvent {
                        text: String::new(),
                        full_text: full,
                        language: "auto".to_string(),
                        is_final: false,
                    },
                );
            };

            // EmitFinal：收尾 is_final 事件（full 取 transcription_text 当前值）。
            let emit_final = || {
                let full = transcription_text.lock().unwrap().clone();
                let _ = app_handle.emit(
                    "asr-transcription",
                    TranscriptionEvent {
                        text: String::new(),
                        full_text: full,
                        language: String::new(),
                        is_final: true,
                    },
                );
            };

            // ── 主 loop：收句 → 喂 pipeline → 按 PuncAction 执行 IO ──
            loop {
                let ready = bridge.punc_ready.load(std::sync::atomic::Ordering::SeqCst);
                match sentence_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok(s) => {
                        let action = pipeline.push(&s, ready);
                        if let punc_pipeline::PuncAction::PassThrough { sentence } = action {
                            passthrough_io(&sentence); // 未就绪：逐句直写（实时）
                        } else {
                            // 就绪：push 返 Nothing，检查达阈值整体标点
                            if let punc_pipeline::PuncAction::RequestPunc { text } =
                                pipeline.decide_flush(
                                    last_flush.elapsed().as_millis() as u64,
                                    ready,
                                )
                            {
                                request_punc_io(&text);
                                last_flush = std::time::Instant::now();
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // stop 信号：进入收尾——持续 drain 等 ASR 转发完 flush 尾段（Disconnected），
                        // 5s grace 兜底（ASR 卡住时不无限等）。就绪态累积待最终 flush，未就绪态逐句直写。
                        if !*running.lock().unwrap() {
                            let grace = std::time::Instant::now() + std::time::Duration::from_secs(5);
                            loop {
                                match sentence_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                                    Ok(s) => {
                                        let action = pipeline.push(
                                            &s,
                                            bridge.punc_ready
                                                .load(std::sync::atomic::Ordering::SeqCst),
                                        );
                                        if let punc_pipeline::PuncAction::PassThrough {
                                            sentence,
                                        } = action
                                        {
                                            passthrough_io(&sentence);
                                        }
                                    }
                                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                                        if std::time::Instant::now() > grace {
                                            break;
                                        }
                                    }
                                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                                }
                            }
                            // finalize：就绪非空 → 最终整体标点 + emit flushed + emit final；
                            // 未就绪/空 → EmitFinal（已逐句直写）。
                            match pipeline
                                .finalize(bridge.punc_ready.load(std::sync::atomic::Ordering::SeqCst))
                            {
                                punc_pipeline::PuncAction::RequestPunc { text } => {
                                    request_punc_io(&text);
                                    emit_final();
                                }
                                _ => emit_final(),
                            }
                            break;
                        }
                        // 非停止（仅就绪态）：静音期有未 flush 原文时按时间兜底 flush。
                        if let punc_pipeline::PuncAction::RequestPunc { text } =
                            pipeline.decide_flush(last_flush.elapsed().as_millis() as u64, ready)
                        {
                            request_punc_io(&text);
                            last_flush = std::time::Instant::now();
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // ASR 线程退出（sentence_tx drop）：drain 残余 + finalize。
                        while let Ok(s) = sentence_rx.try_recv() {
                            let action = pipeline.push(
                                &s,
                                bridge.punc_ready.load(std::sync::atomic::Ordering::SeqCst),
                            );
                            if let punc_pipeline::PuncAction::PassThrough { sentence } = action {
                                passthrough_io(&sentence);
                            }
                        }
                        match pipeline
                            .finalize(bridge.punc_ready.load(std::sync::atomic::Ordering::SeqCst))
                        {
                            punc_pipeline::PuncAction::RequestPunc { text } => {
                                request_punc_io(&text);
                                emit_final();
                            }
                            _ => emit_final(),
                        }
                        break;
                    }
                }
            }

            // drain punc_rx 残留：防跨会话脏消息错配（会话 A 超时滞留的 punc 响应被会话 B
            // 首帧误取致短暂错字）。punc 线程是 punc_rx 唯一消费者，drain 后下次会话干净。
            while bridge.punc_rx.try_recv().is_ok() {}
            *running.lock().unwrap() = false;
            eprintln!("[Punc Thread] 退出");
        })
        .expect("spawn asr-punc");
}

// ── ASR 主线程（feed 子线程 + 主线程双线程解耦 + flush 收尾）──

/// ISSUE-5：丢块告警限流间隔（毫秒）。丢帧期间每 N 秒最多推一次 `asr-warning`，防刷屏。
const DROP_WARN_INTERVAL_MS: u64 = 5000;

/// ISSUE-5：是否推一次丢块告警 `asr-warning`。纯函数（便于单测）。
///
/// `elapsed_ms` = 距上次告警的毫秒；`dropped_delta` = 自上次告起新增丢块数。
/// 无新增 → 不告；有新增 + 达限流间隔 → 告（防刷屏）。
fn should_warn(elapsed_ms: u64, dropped_delta: u64) -> bool {
    dropped_delta > 0 && elapsed_ms >= DROP_WARN_INTERVAL_MS
}

/// ASR 主线程：feed 子线程喂音频 + 主线程收 result/emit（双线程解耦）+ flush 收尾。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_asr_thread(
    app_handle: AppHandle,
    bridge: Arc<python_bridge::PythonAsrBridge>,
    audio_rx: Receiver<Vec<f32>>,
    transcription_text: Arc<Mutex<String>>,
    running: Arc<Mutex<bool>>,
    sentence_tx: Option<Sender<String>>,
    // ISSUE-8：punc 在线时 Some —— emit_result 把原文转发给 punc 线程（punc 独占主字幕）；
    // None —— punc 离线，原文直写主字幕。
    punc_tx: Option<Sender<String>>,
    // ISSUE-5：audio_capture 背压丢块计数 handle，主循环限流 emit `asr-warning`。
    dropped: Arc<std::sync::atomic::AtomicU64>,
) {
    std::thread::spawn(move || {
        eprintln!("[ASR Thread] 启动（feed 子线程喂音频 + 主线程收 result/emit，双线程解耦）");

        // result 处理闭包：主循环 + flush 收尾共用，消除重复。
        // t2s+词典 → F-14 旁路优化 → (punc 在线: 转发 punc / 离线: 原文写主字幕) → emit；error → emit asr-error。
        let emit_result = |r: python_bridge::AsrResult| {
            if !r.text.is_empty() {
                let text = t2s(&dictionary::apply_dictionary(&r.text));
                // F-14：旁路送优化线程（fire-and-forget；满则丢，优化降级）
                if let Some(tx) = &sentence_tx {
                    let _ = tx.try_send(text.clone());
                }
                if let Some(tx) = &punc_tx {
                    // ISSUE-8：punc 在线 → 原文转发给 punc 线程，不直接写主字幕（punc 独占）。
                    let _ = tx.try_send(text);
                } else {
                    // 防御死代码：懒加载后 punc_tx 始终 Some（start_capture 保证），本分支
                    // 不再走到。保留以兼容未来 punc_tx 重变 Option 的纯音频/降级路径。
                    let full = {
                        let mut tt = transcription_text.lock().unwrap();
                        tt.push_str(&text);
                        tt.clone()
                    };
                    eprintln!(
                        "[ASR Thread] 转录: \"{}\" (累计 {} 字符)",
                        if text.chars().count() > 60 {
                            text.chars().take(60).collect::<String>()
                        } else {
                            text.clone()
                        },
                        full.len()
                    );
                    let _ = app_handle.emit(
                        "asr-transcription",
                        TranscriptionEvent {
                            text,
                            full_text: full,
                            language: "auto".to_string(),
                            is_final: r.is_final,
                        },
                    );
                }
            }
            if let Some(e) = r.error {
                let _ = app_handle.emit("asr-error", e);
            }
        };

        // ── Feed 子线程：独占 audio_rx，持续 feed_audio 喂 Python ──
        // 关键：与主线程解耦。feed_audio 同步写 stdin（~140KB/块），Python 做长段 ASR
        // （连续长语音段可达 ~20s，单次 ~10-15s）时不读 stdin → stdin 管道满 → feed_audio
        // 阻塞。旧设计喂音频与收 result 同线程，此阻塞会饿死 try_recv，导致 result 全滞后到
        // stop 后。拆到子线程后，阻塞只影响本线程，主线程持续收 result 实时 emit。
        // 本线程**绝不消费 bridge.result_rx**（crossbeam Receiver 多线程并发 recv 会争抢消息）。
        let bridge_feed = bridge.clone();
        let running_feed = running.clone();
        let (feed_done_tx, feed_done_rx) = bounded::<()>(1);
        let feed_handle = std::thread::Builder::new()
            .name("asr-feed".into())
            .spawn(move || {
                let _tx = feed_done_tx; // 持有至本线程退出，drop → feed_done_rx Disconnected
                loop {
                    if !*running_feed.lock().unwrap() {
                        break;
                    }
                    match audio_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                        Ok(chunk) => {
                            // ISSUE-6：audio_capture 已下采样到 16k（feed 16k，Python librosa 分支短路）
                            if let Err(e) = bridge_feed.feed_audio(&chunk, 16000) {
                                eprintln!(
                                    "[ASR Feed] feed_audio 失败（Python 可能已崩溃）: {}",
                                    e
                                );
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                eprintln!("[ASR Feed] 子线程退出");
            })
            .expect("spawn asr-feed");

        // ── 主循环：收 result 并实时 emit（不被 feed_audio 阻塞）──
        // ISSUE-5：丢块告警限流状态（首次丢块即告警，之后每 DROP_WARN_INTERVAL_MS 一次）
        let mut last_warn: Option<std::time::Instant> = None;
        let mut last_reported: u64 = 0;
        loop {
            // ISSUE-5：限流检查丢块计数 → emit asr-warning（防刷屏）
            {
                let total = dropped.load(std::sync::atomic::Ordering::Relaxed);
                let delta = total.saturating_sub(last_reported);
                let elapsed = last_warn
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(u64::MAX); // 首次视为已超间隔 → 首次丢块即告警
                if should_warn(elapsed, delta) {
                    let _ = app_handle.emit("asr-warning", total);
                    last_warn = Some(std::time::Instant::now());
                    last_reported = total;
                }
            }
            match bridge.result_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(r) => {
                    emit_result(r);
                    // 排干本批已积压的 result
                    while let Ok(r2) = bridge.result_rx.try_recv() {
                        emit_result(r2);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !*running.lock().unwrap() {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        eprintln!("[ASR Thread] 主循环退出，开始收尾");

        // ── 收尾同步：等 feed 子线程退出（超时 3s 保护；超时则 detach 继续）──
        // feed 子线程可能正卡在 feed_audio 的 stdin 写；下面的 flush 会让 Python 读 stdin，
        // feed 子线程随之解除阻塞、回循环顶见 running=false 后退出（退出后不再循环，与主线程
        // 的 request_flush 走 stdin Mutex 串行，无损坏）。
        match feed_done_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(()) => {} // 永不发生（_tx 只持有不 send）
            // Disconnected：feed 子线程已正常退出（_tx drop）—— 预期，静默，不报「未退出」。
            // Timeout：才是真的 3s 未退出（卡在 feed_audio 的 stdin 写），才需告警。
            // 必须区分二者，否则 feed 正常退出（Disconnected）也会被误报为「未退出」。
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                eprintln!("[ASR Thread] 警告：feed 子线程 3s 未退出，detach 继续收尾");
            }
        }

        // ── flush 收尾：触发 Python 末段识别，收到 is_final 提前退出 / deadline 兜底 ──
        // 连续长语音下流式 chunk 末尾仍有未输出的 CIF 积分，stop 不发 exit（bridge 复用），
        // flush action 让 Python 以 is_final=True 刷出末段后异步推回。收到 is_final=True
        // 的 result 即提前退出（解决停止卡顿）；Timeout 继续 / Disconnected 才 break。
        if let Err(e) = bridge.request_flush() {
            eprintln!("[ASR Thread] 发送 flush 失败（Python 可能已退出）: {}", e);
        }
        // flush 收尾：等 Python 把末段识别推回。新流式实现下 flush 末块 is_final=True
        // 的 result 约 0.2-3s 内到达（含 Python 处理积压块的时间），收到即提前退出，
        // 避免正常停止空转 25s（曾致「停止卡住一段时间」）。25s deadline 保留为兜底：
        // 若 Python 异常不推 is_final result（_recognize 异常路径已会推 error），
        // 或长积压下响应滞后，到期解除，永不挂死。Timeout 继续 / Disconnected 才 break。
        let flush_deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
        while std::time::Instant::now() < flush_deadline {
            match bridge.result_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(r) => {
                    let is_final = r.is_final;
                    emit_result(r);
                    // flush 末块 is_final=True 标志收尾完成，提前退出（解决停止卡顿）。
                    if is_final {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        // 兜底 join（feed 子线程通常已在 flush 期间退出）
        let _ = feed_handle.join();

        eprintln!(
            "[ASR Thread] 线程退出，转录文本长度: {}",
            transcription_text.lock().unwrap().len()
        );

        // bridge 是应用级单例（AppState 持有 Arc）：线程的 Arc 引用随闭包结束自然释放，
        // 不关闭 Python 子进程（下次 start_capture 复用）；仅应用退出时 AppState drop 才 shutdown。

        *running.lock().unwrap() = false;

        // 发送最终事件（punc 在线时跳过：punc 线程独占主字幕，负责最终 is_final emit）
        if punc_tx.is_none() {
            let full_text = transcription_text.lock().unwrap().clone();
            if !full_text.is_empty() {
                let event = TranscriptionEvent {
                    text: String::new(),
                    full_text,
                    language: String::new(),
                    is_final: true,
                };
                let _ = app_handle.emit("asr-transcription", event);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_no_drops_false() {
        // 无新增丢块 → 不告警（即使远超间隔）
        assert!(!should_warn(DROP_WARN_INTERVAL_MS * 10, 0));
    }

    #[test]
    fn warn_first_or_after_interval() {
        // 有新增 + 达限流间隔 → 告警（首次 / 窗口外）
        assert!(should_warn(DROP_WARN_INTERVAL_MS, 3));
        assert!(should_warn(DROP_WARN_INTERVAL_MS + 1, 1));
    }

    #[test]
    fn warn_throttled_within_interval() {
        // 有新增但未到间隔 → 不告警（限流防刷屏）
        assert!(!should_warn(DROP_WARN_INTERVAL_MS - 1, 100));
        assert!(!should_warn(0, 5));
    }
}
