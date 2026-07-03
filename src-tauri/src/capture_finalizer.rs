//! VoiceDown - 捕获收尾模块
//!
//! 停止后台收尾：等 capture 线程退出 → drain ASR/优化/punc 运行 flag → 读 ExportConfig
//! 写盘（WAV/TXT/优化文本，逐产物错误隔离，失败不阻塞、永不丢原文）→ emit export-done。
//! 由 `stop_capture` spawn，不阻塞 IPC。A2 候选自 lib.rs 迁出（报告 A After 图
//! `capture_finalizer` deep module）。

use crate::audio_capture::CaptureState;
use crate::export_config::{export_plan, validate_export_dir, ExportConfig};
use crate::json_config_store::JsonConfigStore;
use crate::time_util::timestamp_string;
use crate::{AppState, ExportDone, ExportProgress};
use tauri::{AppHandle, Emitter, Manager};

/// ISSUE-2：停止后台收尾线程。等捕获停 → drain 三 flag（时序/deadline 不变）→ 存盘 → emit done。
/// 不阻塞 IPC（stop_capture 已返回）。导出仍强制（默认路径/txt，ISSUE-4 改读 ExportConfig）。
pub(crate) fn finalize_capture(app_handle: AppHandle) {
    let Some(state) = app_handle.try_state::<AppState>() else {
        eprintln!("[finalize] AppState 不可用，放弃收尾");
        return;
    };

    // 1. 等捕获线程退出（Stopping → Idle/Error）
    loop {
        let stopped = state
            .capturer
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| matches!(c.get_state(), CaptureState::Idle | CaptureState::Error(_)))
            .unwrap_or(true);
        if stopped {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // 2. drain ASR/优化/punc（emit "draining"；时序与 deadline 与旧同步实现一致）
    let _ = app_handle.emit(
        "export-progress",
        ExportProgress {
            phase: "draining".into(),
        },
    );
    #[cfg(feature = "asr")]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let asr_done = state.asr_running.lock().map(|r| !*r).unwrap_or(true);
            let opt_done = state.optimizer_running.lock().map(|r| !*r).unwrap_or(true);
            let punc_done = state.punc_running.lock().map(|r| !*r).unwrap_or(true);
            if (asr_done && opt_done && punc_done) || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    #[cfg(not(feature = "asr"))]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if state.asr_running.lock().map(|r| !*r).unwrap_or(true)
                || std::time::Instant::now() >= deadline
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // 3. 存盘（内部 emit writing-audio / writing-text）+ 构造 done payload
    let done = save_capture_artifacts(&state, &app_handle);
    let _ = app_handle.emit("export-done", done);
    eprintln!("[finalize] 收尾完成");
}

/// 存盘：读 ExportConfig → export_plan 决策 → 逐产物写到 export_dir（逐产物错误隔离，
/// 失败不阻塞其余、永不丢原文）。`enabled=false` 零文件；导出时再 `validate_export_dir`
/// （防保存后被删 / 改权限）。drain 恒跑（在 finalize_capture，转录 UI 完整），本函数仅写盘。
pub(crate) fn save_capture_artifacts(state: &AppState, app_handle: &AppHandle) -> ExportDone {
    let cfg: ExportConfig = JsonConfigStore::default().load("export_config");
    let base = format!("capture_{}", timestamp_string());

    // enabled=false：零文件（drain 已跑完，转录 UI 完整）
    if !cfg.enabled {
        return ExportDone {
            wav_path: None,
            txt_path: None,
            optimized_path: None,
            duration_secs: 0.0,
            skipped: Some("导出未启用".into()),
            error: None,
        };
    }

    // 导出时双校验路径（防保存后被删 / 改权限）：失败 → 不产文件、done 带 error、原文不丢
    let save_dir = match validate_export_dir(&cfg.export_dir) {
        Ok(p) => p,
        Err(e) => {
            return ExportDone {
                wav_path: None,
                txt_path: None,
                optimized_path: None,
                duration_secs: 0.0,
                skipped: None,
                error: Some(format!("导出路径无效: {}", e)),
            };
        }
    };

    // 收集数据（has_audio 用样本数 > 0 判定；转录/优化空字符串 → None）
    let has_audio = {
        let cap = state.capturer.lock().unwrap();
        cap.as_ref()
            .map(|c| c.get_sample_count() > 0)
            .unwrap_or(false)
    };
    let transcription: Option<String> = {
        let t = state.transcription_text.lock().unwrap().clone();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    #[cfg(feature = "asr")]
    let optimized: Option<String> = {
        let o = state.optimized_text.lock().unwrap().clone();
        if o.is_empty() {
            None
        } else {
            Some(o)
        }
    };
    #[cfg(not(feature = "asr"))]
    let optimized: Option<String> = None;

    let plan = export_plan(
        &cfg,
        &base,
        has_audio,
        transcription.as_deref(),
        optimized.as_deref(),
    );

    let mut errors: Vec<String> = Vec::new();
    let mut wav_out: Option<String> = None;
    let mut txt_out: Option<String> = None;
    let mut duration = 0.0_f64;

    // 写音频（plan.wav Some 时）
    if let Some(wav_name) = plan.wav.as_ref() {
        let _ = app_handle.emit(
            "export-progress",
            ExportProgress {
                phase: "writing-audio".into(),
            },
        );
        let wav_path = save_dir.join(wav_name);
        let wav_str = wav_path.to_string_lossy().to_string();
        let cap = state.capturer.lock().unwrap();
        if let Some(capturer) = cap.as_ref() {
            match capturer.save_to_wav(&wav_str) {
                Ok(()) => {
                    let dropped = capturer.get_dropped_count();
                    if dropped > 0 {
                        eprintln!(
                            "[finalize] ⚠ 背压丢块 {}（WAV 存档不受影响，仅 ASR 文本可能缺段）",
                            dropped
                        );
                    }
                    duration = capturer.get_sample_count() as f64 / capturer.get_sample_rate() as f64;
                    wav_out = Some(wav_str.clone());
                    if let Ok(mut p) = state.last_saved_path.lock() {
                        *p = Some(wav_str);
                    }
                }
                Err(e) => errors.push(format!("音频: {}", e)),
            }
        } else {
            errors.push("无捕获器".into());
        }
    }

    // 写文本 + 优化（同一 writing-text 阶段；plan.text 或 plan.optimized Some 时才 emit）
    if plan.text.is_some() || plan.optimized.is_some() {
        let _ = app_handle.emit(
            "export-progress",
            ExportProgress {
                phase: "writing-text".into(),
            },
        );
    }
    if let Some(art) = plan.text.as_ref() {
        let path = save_dir.join(&art.filename);
        match std::fs::write(&path, &art.content) {
            Ok(()) => {
                eprintln!(
                    "[finalize] 转录文本已保存: {} ({} 字符)",
                    path.display(),
                    art.content.len()
                );
                txt_out = Some(path.to_string_lossy().to_string());
            }
            Err(e) => {
                eprintln!("[finalize] 保存转录文本失败: {}", e);
                errors.push(format!("文本: {}", e));
            }
        }
    }
    let optimized_out: Option<String> = {
        #[cfg(feature = "asr")]
        {
            if let Some(art) = plan.optimized.as_ref() {
                let path = save_dir.join(&art.filename);
                match std::fs::write(&path, &art.content) {
                    Ok(()) => {
                        eprintln!(
                            "[finalize] 优化文本已保存: {} ({} 字符)",
                            path.display(),
                            art.content.len()
                        );
                        Some(path.to_string_lossy().to_string())
                    }
                    Err(e) => {
                        eprintln!("[finalize] 保存优化文本失败: {}", e);
                        errors.push(format!("优化文本: {}", e));
                        None
                    }
                }
            } else {
                None
            }
        }
        #[cfg(not(feature = "asr"))]
        {
            None
        }
    };

    // 全无产物且无错误 = 无可导出内容
    let skipped = if wav_out.is_none()
        && txt_out.is_none()
        && optimized_out.is_none()
        && errors.is_empty()
    {
        Some("无可导出内容".into())
    } else {
        None
    };

    ExportDone {
        wav_path: wav_out,
        txt_path: txt_out,
        optimized_path: optimized_out,
        duration_secs: duration,
        skipped,
        error: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
    }
}
