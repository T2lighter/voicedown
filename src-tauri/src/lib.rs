mod text_postprocess;
mod audio_capture;
mod dsp;
mod wav_render;
mod window_selector;
mod time_util;

#[cfg(feature = "asr")]
mod python_bridge;

#[cfg(feature = "asr")]
mod text_optimizer;

#[cfg(feature = "asr")]
mod punc_pipeline;

#[cfg(feature = "asr")]
mod asr_supervisor;

#[cfg(feature = "asr")]
mod asr_session;

// 无条件：导出配置在纯音频模式（--no-default-features）也要可用（⚙ 依赖 asr-gated 的
// get_llm_config，纯音频模式是死的；ExportConfig 独立模块 + 独立 📤 弹窗，不挂 cfg）
mod export_config;
// 无条件：配置持久化层，LLM/导出/词典三处共享（D 候选）。纯音频模式也要可用，不挂 asr。
mod json_config_store;
mod capture_finalizer;
mod dictionary;

use json_config_store::JsonConfigStore;
use audio_capture::{AudioCapturer, CaptureState};
#[cfg(feature = "asr")]
use asr_supervisor::{AsrSupervisor, Phase, RealEmitter, RealSpawner};
use crossbeam_channel::{bounded, Receiver, Sender};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tauri::Manager;
#[cfg(feature = "asr")]
use tauri::Emitter;
use window_selector::WindowInfo;

/// 查找 asr_server.py 脚本路径
///
/// 按优先级尝试多个候选路径，支持 dev 和 production 环境。
#[cfg(feature = "asr")]
fn find_asr_script() -> Option<std::path::PathBuf> {
    // 1. 基于可执行文件路径
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    // 2. 基于当前工作目录（tauri dev 时 CWD = src-tauri/）
    let cwd = std::env::current_dir().ok();

    // 3. 编译时 CARGO_MANIFEST_DIR（cargo dev 时可用）
    let manifest_dir = option_env!("CARGO_MANIFEST_DIR").map(std::path::PathBuf::from);

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Some(ref d) = exe_dir {
        candidates.push(d.join("python_asr").join("asr_server.py"));
    }
    if let Some(ref d) = cwd {
        candidates.push(d.join("python_asr").join("asr_server.py"));
    }
    if let Some(ref d) = manifest_dir {
        candidates.push(d.join("python_asr").join("asr_server.py"));
    }
    candidates.push(std::path::PathBuf::from("python_asr/asr_server.py"));
    candidates.push(std::path::PathBuf::from("../python_asr/asr_server.py"));
    candidates.push(std::path::PathBuf::from("src-tauri/python_asr/asr_server.py"));

    for candidate in &candidates {
        if candidate.exists() {
            eprintln!("[VoiceDown] 找到脚本: {}", candidate.display());
            // 规范化为绝对路径，但避免 Windows UNC 前缀 (\\?\)
            if let Ok(abs) = std::fs::canonicalize(candidate) {
                // 在 Windows 上，canonicalize 可能返回 \\?\ 前缀的路径
                // Python 在某些版本下无法正确处理这种路径，需要转换回普通路径
                #[cfg(windows)]
                {
                    let abs_str = abs.to_string_lossy();
                    if abs_str.starts_with("\\\\?\\") {
                        // 移除 \\?\ 前缀
                        let normal_path = abs_str.strip_prefix("\\\\?\\").unwrap_or(&abs_str);
                        return Some(std::path::PathBuf::from(normal_path));
                    }
                }
                return Some(abs);
            }
            return Some(candidate.clone());
        }
    }

    eprintln!("[VoiceDown] asr_server.py 未找到，尝试过:");
    for c in &candidates {
        eprintln!("  - {}", c.display());
    }
    None
}

// 词典应用层已迁出至 dictionary.rs（A4 候选）。

/// ASR 预加载状态
///
/// 应用全局状态
pub(crate) struct AppState {
    pub(crate) capturer: Mutex<Option<AudioCapturer>>,
    pub(crate) last_saved_path: Mutex<Option<String>>,
    /// 当前累计的完整转录文本
    pub(crate) transcription_text: Arc<Mutex<String>>,
    /// ASR 运行标记
    pub(crate) asr_running: Arc<Mutex<bool>>,
    /// ASR 监管器（应用级单例：启动加载 + 崩溃自愈；start_capture 复用，stop 后保留）
    #[cfg(feature = "asr")]
    asr_bridge: Arc<AsrSupervisor>,
    /// F-14：优化后的完整文本
    #[cfg(feature = "asr")]
    pub(crate) optimized_text: Arc<Mutex<String>>,
    /// F-14：优化线程运行标记（stop 时置 false，线程 drain+flush 后退出并置 false）
    #[cfg(feature = "asr")]
    pub(crate) optimizer_running: Arc<Mutex<bool>>,
    /// ISSUE-8：ct-punc 线程运行标记（punc 在线时独占主字幕；stop 时 drain+最终标点后置 false）
    #[cfg(feature = "asr")]
    pub(crate) punc_running: Arc<Mutex<bool>>,
    /// 离线定稿产物（三级结构化 markdown，停止后手动触发，独立于 optimized_text）
    #[cfg(feature = "asr")]
    final_text: Arc<Mutex<String>>,
    /// 定稿线程运行标记（防重入 CAS）
    #[cfg(feature = "asr")]
    finalizing: Arc<Mutex<bool>>,
}

#[derive(Serialize)]
struct StartResult {
    success: bool,
    message: String,
}

/// 捕获状态（轮询用，供前端控制条分项展示）
#[derive(Serialize)]
struct CaptureStatusInfo {
    samples: u64,
    duration_secs: f64,
    asr_active: bool,
}

#[derive(Serialize)]
struct StopResult {
    success: bool,
    message: String,
    file_path: Option<String>,
    duration_secs: f64,
    /// 完整转录文本（如果有 ASR）
    transcription: Option<String>,
    /// F-14：优化文本路径（仅启用优化时）
    optimized_path: Option<String>,
}

#[derive(Serialize, Clone)]
pub(crate) struct TranscriptionEvent {
    /// 本次新增的文本
    pub(crate) text: String,
    /// 累计完整转录文本
    pub(crate) full_text: String,
    /// 检测到的语言
    pub(crate) language: String,
    /// 是否为最终结果
    pub(crate) is_final: bool,
}

/// F-14：文本优化事件
#[cfg(feature = "asr")]
#[derive(Serialize, Clone)]
pub(crate) struct OptimizeEvent {
    /// 本次新增的优化文本
    pub(crate) optimized: String,
    /// 累计完整优化文本
    pub(crate) full_optimized: String,
    pub(crate) is_final: bool,
}

/// 离线定稿事件（停止后手动触发，单次整篇 LLM 调用产物）
#[cfg(feature = "asr")]
#[derive(Serialize, Clone)]
struct FinalizeEvent {
    /// 三级结构化 markdown 全文
    final_text: String,
    /// _final.md 落盘路径（存盘失败时 None）
    final_path: Option<String>,
}

/// ISSUE-2：导出进度事件（export-progress）。phase = draining | writing-audio | writing-text。
#[derive(Serialize, Clone)]
pub(crate) struct ExportProgress {
    pub(crate) phase: String,
}

/// ISSUE-2：导出完成事件（export-done）。paths=None 表示该项未产出（无数据/失败/未启用）。
#[derive(Serialize, Clone)]
pub(crate) struct ExportDone {
    pub(crate) wav_path: Option<String>,
    pub(crate) txt_path: Option<String>,
    pub(crate) optimized_path: Option<String>,
    pub(crate) duration_secs: f64,
    /// None=正常导出；Some=跳过原因（如「未启用」「无可导出内容」，ISSUE-4 启用）
    pub(crate) skipped: Option<String>,
    /// None=全部成功；Some=部分/全部失败原因（逐产物错误聚合）
    pub(crate) error: Option<String>,
}

// ── IPC 命令 ──────────────────────────────────────────────

/// 枚举可见 Windows 窗口（F-01）
#[tauri::command]
fn list_windows() -> Result<Vec<WindowInfo>, String> {
    Ok(window_selector::list_visible_windows())
}

/// 检查 ASR (paraformer-zh-streaming) 是否可用
#[tauri::command]
fn check_asr_ready() -> Result<bool, String> {
    #[cfg(feature = "asr")]
    {
        // 检查 Python 是否可用
        let has_python = std::process::Command::new("python")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        // 叠加检查关键依赖（funasr/librosa），与 requirements.txt 一致。
        // 避免仅 python 可用就误报 ASR 就绪（前端显示 +ASR 但模型/依赖缺失）。
        let has_deps = std::process::Command::new("python")
            .args(["-c", "import funasr, librosa"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(has_python && has_deps)
    }
    #[cfg(not(feature = "asr"))]
    {
        Ok(false)
    }
}

/// 查询 ASR bridge 当前预加载状态
///
/// 用于前端在注册 `asr-ready`/`asr-error` 监听**之后**主动查询当前状态，
/// 补全可能因"Tauri 事件竞态"（后端 emit 早于前端 listen 注册完成）而丢失的事件。
/// 返回值：`"loading"` | `"ready"` | `"respawning"` | `"error:<msg>"` | `"unavailable"`（非 asr 编译）。
///
/// ISSUE-2：顺便探活——Ready 但 Python 子进程已退出时，触发自动重生并返回 `"respawning"`。
/// 这是 idle 期间检测崩溃的关键路径（前端周期轮询本命令）。
#[tauri::command]
fn get_asr_state(state: tauri::State<'_, AppState>) -> Result<String, String> {
    #[cfg(feature = "asr")]
    {
        let sup = state.asr_bridge.clone();
        // ISSUE-2：Ready 探活——死则触发重生，返回 respawning（idle 崩溃检测关键路径）
        if sup.ensure_alive() {
            return Ok("respawning".to_string());
        }
        Ok(match sup.phase() {
            Phase::Loading => "loading".to_string(),
            Phase::Ready => "ready".to_string(),
            Phase::Respawning { .. } => "respawning".to_string(),
            Phase::Error(e) => format!("error:{}", e),
        })
    }
    #[cfg(not(feature = "asr"))]
    {
        let _ = state;
        Ok("unavailable".to_string())
    }
}

/// ISSUE-2：用户手动「重启 ASR」。仅在 Error 态生效 → 置 Respawning{0} + 后台重生。
#[tauri::command]
fn restart_asr(state: tauri::State<'_, AppState>) -> Result<(), String> {
    #[cfg(feature = "asr")]
    {
        // ISSUE-2：用户手动「重启 ASR」。仅 Error 态生效 → Respawning{0} + 后台重生（supervisor 内部）。
        state.asr_bridge.restart();
        Ok(())
    }
    #[cfg(not(feature = "asr"))]
    {
        let _ = state;
        Ok(())
    }
}

/// 获取当前累计的转录文本
#[tauri::command]
fn get_transcription(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let text = state.transcription_text.lock().map_err(|e| e.to_string())?;
    Ok(text.clone())
}

/// ISSUE-4：读取用户词典（key→value 快照，前端编辑用）。
#[tauri::command]
fn get_dictionary() -> Result<BTreeMap<String, String>, String> {
    Ok(dictionary::get_dictionary_snapshot())
}

/// ISSUE-4：保存用户词典（持久化到 dictionary.json + 内存即时生效）。
#[tauri::command]
fn set_dictionary(dict: BTreeMap<String, String>) -> Result<(), String> {
    dictionary::store_dictionary(dict)
}

/// F-14：读取 LLM 优化配置
#[cfg(feature = "asr")]
#[tauri::command]
fn get_llm_config() -> Result<text_optimizer::LlmConfig, String> {
    Ok(JsonConfigStore::default().load("llm_config"))
}

/// F-14：保存 LLM 优化配置
#[cfg(feature = "asr")]
#[tauri::command]
fn set_llm_config(config: text_optimizer::LlmConfig) -> Result<(), String> {
    JsonConfigStore::default().save("llm_config", &config)
}

/// F-14：探测当前配置的后端是否可用
#[cfg(feature = "asr")]
#[tauri::command]
fn check_llm_available() -> Result<bool, String> {
    let cfg: text_optimizer::LlmConfig = JsonConfigStore::default().load("llm_config");
    match text_optimizer::build_backend(&cfg) {
        Some(b) => Ok(b.check_available()),
        None => Ok(false),
    }
}

/// 离线定稿：停止后手动触发，单次整篇 LLM 调用 → 三级结构化 markdown。
/// 防重入 CAS 后 spawn 单次线程（fire-and-forget），命令立即返回 Ok，结果走事件。
/// 输入源：optimized_text 优先（pick_finalize_input）；失败只 emit warn，不丢原文/优化文。
#[cfg(feature = "asr")]
#[tauri::command]
fn finalize_document(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // 1. 防重入 CAS：finalizing false→true
    {
        let mut f = state.finalizing.lock().map_err(|e| e.to_string())?;
        if *f {
            return Err("定稿进行中，请等待完成".into());
        }
        *f = true;
    }
    // 2. 校验：未在捕获（捕获中拒绝，回滚 CAS）
    if *state.asr_running.lock().map_err(|e| e.to_string())? {
        if let Ok(mut f) = state.finalizing.lock() {
            *f = false;
        }
        return Err("捕获中无法定稿，请先停止".into());
    }
    // 3. 取输入：optimized_text 优先，空则 None（ISSUE-1 范围不 fallback 到转录）
    let input = {
        let opt = state.optimized_text.lock().map_err(|e| e.to_string())?;
        let tr = state.transcription_text.lock().map_err(|e| e.to_string())?;
        match text_optimizer::pick_finalize_input(&opt, &tr) {
            Some(s) => s,
            None => {
                if let Ok(mut f) = state.finalizing.lock() {
                    *f = false;
                }
                return Err("无文本可定稿".into());
            }
        }
    };
    // 4. last_saved_path（_final.md 命名：capture_<ts>.wav → _final.md）
    let last_saved_path = state.last_saved_path.lock().map_err(|e| e.to_string())?.clone();
    // 5. spawn 单次线程（fire-and-forget，结果走事件，不阻塞 IPC）
    let final_text = state.final_text.clone();
    let finalizing = state.finalizing.clone();
    std::thread::Builder::new()
        .name("asr-finalize".into())
        .spawn(move || {
            let cleanup = || {
                if let Ok(mut f) = finalizing.lock() {
                    *f = false;
                }
            };
            let cfg: text_optimizer::LlmConfig = JsonConfigStore::default().load("llm_config");
            let backend = match text_optimizer::build_backend(&cfg) {
                Some(b) => b,
                None => {
                    eprintln!("[Finalize] LLM 未启用或后端不可用");
                    let _ = app_handle.emit("asr-finalize-warn", "LLM 未启用或后端不可用".to_string());
                    cleanup();
                    return;
                }
            };
            eprintln!("[Finalize] 开始整篇定稿 ({} 字)", input.chars().count());
            match backend.optimize(&input, "finalize") {
                Ok(md) => {
                    {
                        let mut t = final_text.lock().unwrap();
                        *t = md.clone();
                    }
                    let final_path = last_saved_path.as_ref().and_then(|wav| {
                        let p = wav.replace(".wav", "_final.md");
                        match std::fs::write(&p, &md) {
                            Ok(_) => {
                                eprintln!("[Finalize] 定稿已保存: {}", p);
                                Some(p)
                            }
                            Err(e) => {
                                eprintln!("[Finalize] 存盘失败: {}", e);
                                None
                            }
                        }
                    });
                    let _ = app_handle.emit(
                        "asr-finalize",
                        FinalizeEvent { final_text: md, final_path },
                    );
                    eprintln!("[Finalize] 定稿完成");
                }
                Err(e) => {
                    eprintln!("[Finalize] LLM 调用失败: {}", e);
                    let _ = app_handle.emit("asr-finalize-warn", format!("定稿失败: {}", e));
                }
            }
            cleanup();
        })
        .map_err(|e| {
            if let Ok(mut f) = state.finalizing.lock() {
                *f = false;
            }
            e.to_string()
        })?;
    Ok(())
}

/// ISSUE-3：读取导出配置（无条件——纯音频模式也要可用）
#[tauri::command]
fn get_export_config() -> Result<export_config::ExportConfig, String> {
    Ok(JsonConfigStore::default().load("export_config"))
}

/// ISSUE-3：保存导出配置（保存时双校验路径，PRD；无条件）
#[tauri::command]
fn set_export_config(config: export_config::ExportConfig) -> Result<(), String> {
    export_config::validate_export_dir(&config.export_dir).map_err(|e| e.to_string())?;
    JsonConfigStore::default().save("export_config", &config)
}

/// ISSUE-3：实时校验导出路径（前端弹窗红字提示用；无条件）
#[tauri::command]
fn validate_export_path(path: String) -> Result<(), String> {
    export_config::validate_export_dir(&path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 启动 WASAPI 进程级 Loopback 音频捕获 + ASR 转录
#[tauri::command]
fn start_capture(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    pid: u32,
) -> Result<StartResult, String> {
    // 始终使用 include_tree=true（捕获目标进程及其子进程）
    // 保留参数签名以便兼容可能的未来扩展
    let _include_tree = true;
    // asr feature：先检查预加载的 bridge 状态（Loading/Respawning/Error 时拒绝，避免无谓捕获）
    #[cfg(feature = "asr")]
    {
        let sup = state.asr_bridge.clone();
        match sup.phase() {
            Phase::Loading => {
                return Err("ASR 正在加载中，请稍候（就绪后自动启用「开始」）".into());
            }
            Phase::Respawning { .. } => {
                return Err("ASR 正在自动重启，请稍后重试".into());
            }
            Phase::Error(e) => {
                return Err(format!("ASR 加载失败，无法转录: {e}"));
            }
            Phase::Ready => {} // 就绪，继续探活
        }
        // ISSUE-2：Ready 但子进程已退出 → 触发自动重生，拒绝本次（用户稍后重试）
        if sup.ensure_alive() {
            return Err("ASR 进程已退出，正在自动重启，请稍后重试".into());
        }
    }

    // 创建音频通道（缓冲 3 个分片）
    let (audio_chunk_tx, audio_chunk_rx): (Sender<Vec<f32>>, Receiver<Vec<f32>>) =
        bounded::<Vec<f32>>(3);

    // 创建带 ASR 通道的音频捕获器
    let capturer = AudioCapturer::new_with_asr(pid, _include_tree, audio_chunk_tx);
    let dropped_handle = capturer.dropped_handle(); // ISSUE-5：丢块计数 handle，传 ASR 主线程限流 emit
    capturer.start_capturing().map_err(|e| e.to_string())?;

    {
        let mut cap = state.capturer.lock().map_err(|e| e.to_string())?;
        *cap = Some(capturer);
    }

    // asr feature：复用预加载的 bridge，启动流式 ASR 线程
    #[cfg(feature = "asr")]
    {
        let bridge = state.asr_bridge.ready_bridge().ok_or_else(|| {
            "ASR 未就绪（bridge 不可用，可能刚触发重生），请稍后重试".to_string()
        })?;
        eprintln!("[lib] ASR 已就绪（预加载），启动 ASR 线程");
        {
            let mut text = state.transcription_text.lock().map_err(|e| e.to_string())?;
            text.clear();
        }
        {
            let mut running = state.asr_running.lock().map_err(|e| e.to_string())?;
            *running = true;
        }
        // F-14：加载 LLM 配置，启用则启动优化线程
        let cfg: text_optimizer::LlmConfig = JsonConfigStore::default().load("llm_config");
        let backend = text_optimizer::build_backend(&cfg);
        let (sentence_tx, sentence_rx) = if backend.is_some() {
            let (tx, rx) = bounded::<String>(16);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        if let (Some(backend), Some(rx)) = (backend, sentence_rx) {
            {
                let mut ot = state.optimized_text.lock().map_err(|e| e.to_string())?;
                ot.clear();
            }
            {
                let mut or = state.optimizer_running.lock().map_err(|e| e.to_string())?;
                *or = true;
            }
            eprintln!("[lib] 文本优化已启用 (backend={})", backend.name());
            asr_session::spawn_optimizer_thread(
                app_handle.clone(),
                rx,
                state.optimized_text.clone(),
                state.optimizer_running.clone(),
                backend,
            );
        } else {
            eprintln!("[lib] 文本优化未启用或后端不可用");
        }

        // 懒加载：punc 线程始终起（ct-punc 后台加载，bridge.punc_ready 初始 false）。
        // ASR 原文转发给 punc 线程；punc 线程按 bridge.punc_ready 决定走 ct-punc（就绪）
        // 还是原文逐句直写（未就绪/降级，复刻离线降级实时性）。
        let (punc_tx, punc_rx_ch) = bounded::<String>(64);
        {
            let mut pr = state.punc_running.lock().map_err(|e| e.to_string())?;
            *pr = true;
        }
        asr_session::spawn_punc_thread(
            app_handle.clone(),
            bridge.clone(),
            punc_rx_ch,
            state.transcription_text.clone(),
            state.punc_running.clone(),
        );
        eprintln!("[lib] punc 线程已起（ct-punc 后台懒加载，未就绪时主字幕原文逐句直写）");

        asr_session::spawn_asr_thread(
            app_handle,
            bridge,
            audio_chunk_rx,
            state.transcription_text.clone(),
            state.asr_running.clone(),
            sentence_tx,
            Some(punc_tx),
            dropped_handle,
        );
    }
    #[cfg(not(feature = "asr"))]
    {
        eprintln!("[lib] ASR 未启用");
        drop(audio_chunk_rx);
        drop(dropped_handle);
    }

    let mut msg = format!(
        "进程级音频捕获已启动 (PID={})\n将只捕获该进程及其子进程的音频输出。",
        pid
    );
    #[cfg(feature = "asr")]
    msg.push_str("\n语音转文字 (ParaformerStreaming) 已同步启动。");

    Ok(StartResult {
        success: true,
        message: msg,
    })
}

/// 停止捕获（异步）：立刻返回 ack，drain + 存盘在后台线程完成，前端靠事件驱动。
///
/// ISSUE-2：不再 await drain（那是卡顿根因）。本函数：置捕获 Stopping + 信号三线程 +
/// spawn finalize_capture → 立刻返回。finalize 线程：等捕获停 → emit draining → drain →
/// emit writing-audio/writing-text → 存盘 → emit export-done。
#[tauri::command]
fn stop_capture(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<StopResult, String> {
    // 置 Stopping（立即生效；二次 stop 见非 Capturing 态被拒，防重复 finalize）
    {
        let cap = state.capturer.lock().map_err(|e| e.to_string())?;
        match &*cap {
            None => return Err("当前没有进行的捕获".to_string()),
            Some(c) => {
                if c.get_state() != CaptureState::Capturing {
                    return Err("当前未在捕获".to_string());
                }
                c.stop_capturing();
            }
        }
    }

    // 0. 信号 ASR/优化/punc 线程收尾（各线程自行 drain 后置 flag=false）
    {
        let mut running = state.asr_running.lock().map_err(|e| e.to_string())?;
        *running = false;
    }
    #[cfg(feature = "asr")]
    {
        let mut opt_running = state.optimizer_running.lock().map_err(|e| e.to_string())?;
        *opt_running = false;
        let mut pr = state.punc_running.lock().map_err(|e| e.to_string())?;
        *pr = false;
    }

    // 后台 finalize（drain + 存盘 + emit），不阻塞 IPC
    let handle = app_handle.clone();
    std::thread::Builder::new()
        .name("capture-finalize".into())
        .spawn(move || capture_finalizer::finalize_capture(handle))
        .map_err(|e| format!("启动导出线程失败: {}", e))?;

    Ok(StopResult {
        success: true,
        message: "捕获停止中，正在导出…".into(),
        file_path: None,
        duration_secs: 0.0,
        transcription: None,
        optimized_path: None,
    })
}

// finalize_capture + save_capture_artifacts 已迁出至 capture_finalizer.rs（A2 候选）。

/// 获取最近一次保存的文件路径
#[tauri::command]
fn get_last_saved_path(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let path = state.last_saved_path.lock().map_err(|e| e.to_string())?;
    Ok(path.clone())
}

/// 轮询当前捕获状态（结构化，供前端控制条分项展示）
#[tauri::command]
fn get_capture_status(state: tauri::State<'_, AppState>) -> Result<CaptureStatusInfo, String> {
    let cap = state.capturer.lock().map_err(|e| e.to_string())?;
    match *cap {
        Some(ref capturer) => {
            let samples = capturer.get_sample_count() as u64;
            let duration_secs = samples as f64 / capturer.get_sample_rate() as f64;
            let asr_active = { state.asr_running.lock().map(|r| *r).unwrap_or(false) };
            Ok(CaptureStatusInfo {
                samples,
                duration_secs,
                asr_active,
            })
        }
        None => Ok(CaptureStatusInfo {
            samples: 0,
            duration_secs: 0.0,
            asr_active: false,
        }),
    }
}

// ── F-14 文本优化线程 ───────────────────────────────────

/// flush 一批：overlap 构造输入（上批末句+新句）→ 调 LLM 重新分段 → 更新 rewriter → emit full。
// flush_optimizer + spawn_optimizer_thread 已迁出至 asr_session.rs（A3-a1 候选）。

// ASR 三线程编排（optimizer/punc/asr + should_warn）已迁出至 asr_session.rs（A3 候选）。

// ── 应用入口 ──────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let t0 = std::time::Instant::now();
            // 窗口背景设为 App 主题色 #1c1c1e（与 index.html/App.css 一致），避免黑屏色差跳变。
            if let Some(window) = app.get_webview_window("main") {
                let _ =
                    window.set_background_color(Some(tauri::window::Color(28, 28, 30, 255)));
            }

            // 注：setup 中原同步调用 check_asr_ready() 已移除以消除主线程阻塞
            // 该调用无功能依赖（AppState.asr_bridge 初始即 Loading，由后台 preload_asr 更新）
            #[cfg(not(feature = "asr"))]
            eprintln!("[VoiceDown] ASR 未启用（编译时未开启 asr feature）");

            // asr feature：构造监管器需 save_dir（stderr 落盘 logs/asr_<ts>.log，ISSUE-5）
            #[cfg(feature = "asr")]
            let save_dir = {
                let up = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
                format!("{}\\Documents\\VoiceDown", up)
            };
            app.manage(AppState {
                capturer: Mutex::new(None),
                last_saved_path: Mutex::new(None),
                transcription_text: Arc::new(Mutex::new(String::new())),
                asr_running: Arc::new(Mutex::new(false)),
                #[cfg(feature = "asr")]
                asr_bridge: AsrSupervisor::new(
                    find_asr_script().map(|p| p.to_string_lossy().to_string()),
                    save_dir.clone(),
                    Box::new(RealSpawner),
                    Box::new(RealEmitter::new(app.handle().clone())),
                ),
                #[cfg(feature = "asr")]
                optimized_text: Arc::new(Mutex::new(String::new())),
                #[cfg(feature = "asr")]
                optimizer_running: Arc::new(Mutex::new(false)),
                #[cfg(feature = "asr")]
                punc_running: Arc::new(Mutex::new(false)),
                #[cfg(feature = "asr")]
                final_text: Arc::new(Mutex::new(String::new())),
                #[cfg(feature = "asr")]
                finalizing: Arc::new(Mutex::new(false)),
            });

            // asr feature：后台预加载（懒加载：只加 paraformer ready ~15s / 首跑下载 ~90s，ct-punc 后台加，不阻塞应用启动/UI）
            #[cfg(feature = "asr")]
            {
                let sup = app
                    .handle()
                    .try_state::<AppState>()
                    .expect("AppState 刚 manage")
                    .asr_bridge
                    .clone();
                std::thread::spawn(move || sup.spawn_initial());
            }

            eprintln!("[VoiceDown] setup 完成 @ {:?}（WebView 开始加载）", t0.elapsed());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_windows,
            start_capture,
            stop_capture,
            get_capture_status,
            get_last_saved_path,
            check_asr_ready,
            get_asr_state,
            restart_asr,
            get_transcription,
            get_dictionary,
            set_dictionary,
            #[cfg(feature = "asr")]
            get_llm_config,
            #[cfg(feature = "asr")]
            set_llm_config,
            #[cfg(feature = "asr")]
            check_llm_available,
            #[cfg(feature = "asr")]
            finalize_document,
            get_export_config,
            set_export_config,
            validate_export_path,
        ])
        .run(tauri::generate_context!())
        .expect("启动 VoiceDown 失败");
}

#[cfg(all(test, feature = "asr"))]
mod tests {
    use super::*;

    // ── ISSUE-2：export-done 事件契约（Rust↔JS IPC 形状，前端 export-done 监听依赖此形状）──
    #[test]
    fn export_done_serializes_success_shape() {
        let done = ExportDone {
            wav_path: Some("C:\\a.wav".into()),
            txt_path: Some("C:\\a.txt".into()),
            optimized_path: None,
            duration_secs: 12.5,
            skipped: None,
            error: None,
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains("\"wav_path\":\"C:\\\\a.wav\""), "{}", json);
        assert!(json.contains("\"txt_path\":\"C:\\\\a.txt\""), "{}", json);
        assert!(json.contains("\"optimized_path\":null"), "{}", json);
        assert!(json.contains("\"duration_secs\":12.5"), "{}", json);
        assert!(json.contains("\"skipped\":null"), "{}", json);
        assert!(json.contains("\"error\":null"), "{}", json);
    }

    #[test]
    fn export_done_serializes_error_shape() {
        let done = ExportDone {
            wav_path: None,
            txt_path: None,
            optimized_path: None,
            duration_secs: 0.0,
            skipped: Some("未启用".into()),
            error: Some("音频: 磁盘满".into()),
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains("\"wav_path\":null"), "{}", json);
        assert!(json.contains("\"skipped\":\"未启用\""), "{}", json);
        assert!(json.contains("\"error\":\"音频: 磁盘满\""), "{}", json);
    }
}
