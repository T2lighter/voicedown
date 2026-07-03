/// VoiceDown - Python ASR 子进程管理模块
///
/// 管理 paraformer-zh-streaming 流式模型的 Python 子进程生命周期。
/// 通过 stdin/stdout JSON lines 协议通信（流式推送）。
///
/// ## 协议
/// - 请求 (Rust → Python stdin):
///   - 喂音频: {"action": "feed_audio", "audio_data": [...], "sample_rate": 48000}（不等响应）
///   - 末段收尾: {"action": "flush"}（stop_capture 时触发，不退进程，保 bridge 复用）
///   - 退出: {"action": "exit"}
/// - 响应 (Python stdout → Rust，由独立读取线程独占读取并推入 channel):
///   - 首行 ready: {"status":"ready","models_loaded":true,"punc_loaded":false}（paraformer 加载后输出，首跑下载 ~90s，缓存后 ~15s；ct-punc 后台懒加载，punc_loaded 初始 false）
///   - 识别结果: {"text": "识别结果", "is_final": true, "error": null}
///
/// ## 并发模型
/// stdout 由独立读取线程独占（`BufReader<ChildStdout>` 不可跨线程共享），
/// 解析后推 `AsrResult` 到 crossbeam channel。`feed_audio` 只写 stdin；
/// 结果由调用方（lib.rs 的 ASR 线程）从 `result_rx` 非阻塞轮询。
///
/// ## 生命周期
/// PythonAsrBridge::spawn() → 启动 Python 子进程 → 同步等首行 ready
///   → feed_audio
///   → shutdown() / Drop → 发 exit，等待子进程退出

use crossbeam_channel::{unbounded, Receiver as Rcv};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

// ── 错误类型 ──────────────────────────────────────────────

#[derive(Debug)]
pub enum BridgeError {
    /// Python 未安装或不在 PATH 中
    PythonNotFound(String),
    /// 子进程启动失败
    SpawnFailed(String),
    /// 子进程意外退出
    ProcessExited(String),
    /// JSON 序列化/反序列化错误
    JsonError(String),
    /// stdin/stdout 通信错误
    IoError(String),
    /// Python 端返回的业务错误
    AsrError(String),
    /// 模型加载超时
    LoadTimeout(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PythonNotFound(e) => write!(f, "Python 未找到: {e}"),
            Self::SpawnFailed(e) => write!(f, "子进程启动失败: {e}"),
            Self::ProcessExited(e) => write!(f, "子进程异常退出: {e}"),
            Self::JsonError(e) => write!(f, "JSON 错误: {e}"),
            Self::IoError(e) => write!(f, "IO 错误: {e}"),
            Self::AsrError(e) => write!(f, "ASR 错误: {e}"),
            Self::LoadTimeout(e) => write!(f, "模型加载超时: {e}"),
        }
    }
}

impl std::error::Error for BridgeError {}

// ── 响应/结果类型 ─────────────────────────────────────────

/// 流式 ASR 结果（由 stdout 读取线程解析后推入 channel）
#[derive(Debug, Clone)]
pub struct AsrResult {
    pub text: String,
    pub is_final: bool,
    pub error: Option<String>,
}

/// ct-punc 标点恢复结果（ISSUE-8；stdout 读取线程按 `type=punc` 分流到 `punc_rx`，
/// 与流式 ASR result 分离，避免两类响应在 `result_rx` 争抢）。
#[derive(Debug, Clone)]
pub struct PuncResult {
    pub text: String,
    pub error: Option<String>,
}

// ── Python 子进程管理器 ───────────────────────────────────

/// Python 子进程桥接器
///
/// 持有子进程句柄、stdin 互斥锁与 stdout 结果 channel。
/// stdout 由独立读取线程独占读取，解析后推入 `result_rx`。
pub struct PythonAsrBridge {
    // Mutex 包装以支持从共享引用（&self，经 Arc 跨线程）探活：
    // Child::try_wait 需 &mut self，健康检查线程 / get_asr_state 探活需经 Mutex 拿可变访问。
    child: Mutex<Child>,
    stdin: Mutex<std::process::ChildStdin>,
    pub result_rx: Rcv<AsrResult>,
    /// ct-punc 标点结果通道（punc 线程独占消费）。
    pub punc_rx: Rcv<PuncResult>,
    /// ct-punc 是否就绪（后台懒加载）。spawn 时 false；read_stdout_loop 收 punc_ready 信号翻 true。
    /// punc 线程始终起，按此标志决定走 ct-punc（就绪）还是原文逐句直写（未就绪/降级）。
    pub punc_ready: Arc<std::sync::atomic::AtomicBool>,
    #[allow(dead_code)]
    python_script: PathBuf,
}

impl PythonAsrBridge {
    /// 启动 Python 子进程并等待模型加载完成（同步读首行 ready）
    ///
    /// `python_script` 为 asr_server.py 的路径。
    /// 会先检查 Python 是否可用，然后启动子进程，同步阻塞读 stdout 首行 ready，
    /// 成功后启动读取线程接管 stdout。
    pub fn spawn(python_script: &str, log_dir: &str) -> Result<Self, BridgeError> {
        // 1. 检查 Python 是否可用
        let python = find_python()?;

        // 2. 检查脚本是否存在
        if !std::path::Path::new(python_script).exists() {
            return Err(BridgeError::SpawnFailed(format!(
                "Python 脚本不存在: {}",
                python_script
            )));
        }

        // 3. 获取脚本所在目录（以便 Python import 工作目录正确）
        let script_path = std::path::Path::new(python_script);
        let script_dir = script_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));

        eprintln!(
            "[PythonBridge] 启动 Python 子进程: {} {}",
            python, python_script
        );

        // 4. 启动 Python 子进程
        let mut child = Command::new(&python)
            .arg(python_script)
            .current_dir(script_dir)
            .env("PYTHONIOENCODING", "utf-8") // ⚠️ 必须！否则 Windows CP936 破坏 JSON
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // ISSUE-5：落盘 logs/asr_<ts>.log（打包无控制台可排障）
            .spawn()
            .map_err(|e| BridgeError::SpawnFailed(format!("{}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BridgeError::SpawnFailed("无法获取 stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BridgeError::SpawnFailed("无法获取 stdout".into()))?;

        // ISSUE-5：stderr 落盘（REM-05）。打包 .exe 无控制台，stderr 改 piped 由独立线程按行
        // 追加到 logs/asr_<ts>.log。Python 端 suppress_stdout 只重定向 fd1，fd2 不受影响，故
        // 不与 stdout 的 JSON 协议冲突；本线程只落盘、绝不解析协议（独占 BufReader）。
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BridgeError::SpawnFailed("无法获取 stderr".into()))?;
        let log_path = stderr_log_path(log_dir);
        // ISSUE-5：确保 logs 目录存在（否则 open 失败，stderr 仅排空不落盘）。
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::thread::spawn(move || read_stderr_loop(BufReader::new(stderr), log_path));

        // 5. 同步等首行 ready（paraformer 加载，缓存命中 ~15s，首跑下载 ~90s，超时 180s）。
        //    懒加载：ready 行 punc_loaded 恒 false（ct-punc 后台加载尚未就绪）。返回值仅供
        //    日志——punc 能力不再由 spawn 时定型，改由运行时 punc_ready 信号驱动（见下）。
        let mut reader = BufReader::new(stdout);
        let _ready_punc_loaded = wait_first_ready(&mut reader, 180)?;

        // 6. punc_ready：ct-punc 后台懒加载就绪标志。spawn 时 false，read_stdout_loop 收
        //    punc_ready 信号翻 true。punc 线程始终起（start_capture），按此标志决定走 ct-punc
        //    还是原文逐句直写。Arc 跨线程共享：read_stdout_loop 写 / punc 线程读。
        let punc_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 7. 启动 stdout 读取线程（独占 reader），按 type 分流：流式 ASR result → result_rx，
        //    ct-punc result → punc_rx，punc_ready 信号 → 翻 punc_ready 标志（三类不争抢）。
        let (result_tx, result_rx) = unbounded::<AsrResult>();
        let (punc_tx, punc_rx) = unbounded::<PuncResult>();
        let punc_ready_reader = punc_ready.clone();
        std::thread::spawn(move || {
            read_stdout_loop(reader, result_tx, punc_tx, punc_ready_reader)
        });

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            result_rx,
            punc_rx,
            punc_ready,
            python_script: PathBuf::from(python_script),
        })
    }

    /// 喂音频（流式，不等响应）
    ///
    /// 向 Python stdin 发送 `feed_audio` 请求。结果由 Python 异步通过 stdout 推送，
    /// 由读取线程转入 `result_rx`。
    pub fn feed_audio(&self, audio: &[f32], sample_rate: u32) -> Result<(), BridgeError> {
        let req = serde_json::json!({
            "action": "feed_audio",
            "audio_data": audio,
            "sample_rate": sample_rate,
        });
        let s = serde_json::to_string(&req).map_err(|e| BridgeError::JsonError(e.to_string()))?;
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        writeln!(stdin, "{s}").map_err(|e| BridgeError::IoError(e.to_string()))?;
        stdin
            .flush()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        Ok(())
    }

    /// 触发 Python 端末段收尾识别（流式收尾，不退出进程）
    ///
    /// 发送 `flush` action：Python 端 `flush_stream` 把缓冲内剩余尾巴作为末块
    /// 以 is_final=True 喂 ParaformerStreaming，强制刷出 cache 内剩余 CIF 积分
    /// 对应的尾文本，并通过 stdout 异步推送结果（见 `result_rx`）。
    ///
    /// `stop_capture` 时调用，避免连续长语音场景下流式 chunk 末尾仍有未输出的
    /// CIF 积分、exit 又不发（bridge 复用），导致末段（甚至全部）文字丢失。
    ///
    /// 与 `exit` 的区别：flush 不结束 Python 子进程，bridge 仍可复用于下次会话；
    /// `flush_stream` 末尾 reset_stream 清空 `_cache` + buffer，下次会话从干净状态开始。
    pub fn request_flush(&self) -> Result<(), BridgeError> {
        let s = serde_json::json!({"action": "flush"}).to_string();
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        writeln!(stdin, "{s}").map_err(|e| BridgeError::IoError(e.to_string()))?;
        stdin
            .flush()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        Ok(())
    }

    /// 请求 ct-punc 标点恢复（ISSUE-8，请求-响应模式）。
    ///
    /// 发送 `{"action":"punc","text":...}`；Python 端 `punctuate` 后回
    /// `{"type":"punc","text":...,"error":...}`，由 stdout 读取线程分流到 `punc_rx`。
    /// 调用方（punc 线程）写后从 `punc_rx.recv_timeout` 取响应（punc 线程是 punc_rx 唯一消费者，
    /// 串行一问一答，无乱序）。
    pub fn request_punc(&self, text: &str) -> Result<(), BridgeError> {
        let s = serde_json::json!({"action": "punc", "text": text}).to_string();
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        writeln!(stdin, "{s}").map_err(|e| BridgeError::IoError(e.to_string()))?;
        stdin
            .flush()
            .map_err(|e| BridgeError::IoError(e.to_string()))?;
        Ok(())
    }

    /// 探活：子进程是否仍在运行（ISSUE-2 崩溃自愈用）。
    ///
    /// `try_wait` 返回 `Ok(None)` = 仍在运行；`Ok(Some)` = 已退出；`Err` = 查询失败。
    /// 仅在「已确认退出」时返回 false；查询失败（极罕见）视为存活，避免误杀健康进程。
    /// 与 `feed_audio`(锁 stdin) / `shutdown`(锁 child) 的锁顺序一致：仅锁 child，不嵌套。
    pub fn is_alive(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|pe| pe.into_inner());
        !matches!(child.try_wait(), Ok(Some(_)))
    }

    /// 关闭 Python 子进程
    pub fn shutdown(&mut self) {
        eprintln!("[PythonBridge] 关闭 Python 子进程...");
        // 发送 exit 命令
        {
            if let Ok(mut stdin) = self.stdin.lock() {
                let _ = writeln!(stdin, "{{\"action\":\"exit\"}}");
                let _ = stdin.flush();
            }
        }
        // 等待子进程退出（最多 5 秒）
        let mut child = self.child.lock().unwrap_or_else(|pe| pe.into_inner());
        let timeout = std::time::Duration::from_secs(5);
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!(
                        "[PythonBridge] Python 进程已退出 (exit: {:?})",
                        status.code()
                    );
                    return;
                }
                Ok(None) => {
                    if start.elapsed() > timeout {
                        eprintln!("[PythonBridge] 超时，强制终止 Python 进程");
                        let _ = child.kill();
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("[PythonBridge] 等待进程退出失败: {}", e);
                    let _ = child.kill();
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

impl Drop for PythonAsrBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── stderr 落盘线程（ISSUE-5 / REM-05）────────────────────

/// ISSUE-5：stderr 日志路径 `{log_dir}\logs\asr_<epoch_secs>.log`（按会话一个文件）。
fn stderr_log_path(log_dir: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("{}\\logs\\asr_{}.log", log_dir, ts))
}

/// ISSUE-5：stderr 读取循环（独占 reader）。**无论日志文件能否打开，都必须读尽 reader**：
/// 提前 return 会让 reader drop → 关闭 pipe 读端 → Python 写 stderr 报 [Errno 22] →
/// load_models 崩溃（首发 bug 根因）。文件 open 失败时仅排空不落盘，但仍读尽。
fn read_stderr_loop(reader: BufReader<std::process::ChildStderr>, log_path: PathBuf) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    drain_stderr(reader, file);
}

/// ISSUE-5：读尽 `reader`（到 EOF），每行 append 到 `file`（若有）。抽为独立纯逻辑便于单测；
/// `file=None`（日志 open 失败）时仅排空不落盘，但**仍读尽**——pipe 读端不关闭的铁律。
fn drain_stderr<R: std::io::BufRead, W: std::io::Write>(mut reader: R, mut file: Option<W>) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF：Python 进程已关闭 stderr
            Ok(_) => {
                if let Some(ref mut f) = file {
                    let _ = f.write_all(line.as_bytes());
                }
            }
            Err(_) => break,
        }
    }
}

// ── stdout 行解析（纯函数，便于单测）──────────────────────

/// 一行 stdout JSON 解析结果，按 `type` 分流。
#[derive(Debug, Clone)]
pub enum StdoutMsg {
    /// 流式 ASR 结果（含 text/is_final/error 之一）。
    Result(AsrResult),
    /// ct-punc 标点响应（type=punc）。
    Punc(PuncResult),
    /// ct-punc 后台懒加载就绪信号（type=punc_ready）。ready=true 就绪 / false 失败降级。
    /// 失败的 error 字段不外泄（仅落日志），防误触发 asr-error（红线：ct-punc 是旁路）。
    PuncReady(bool),
    /// 控制消息（ready/ping）或非 JSON 行，跳过。
    Skip,
}

/// 解析一行 stdout JSON 为 StdoutMsg。纯函数（无 IO），便于单测覆盖各分流分支。
pub fn parse_stdout_line(line: &str) -> StdoutMsg {
    let v = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => return StdoutMsg::Skip, // 非 JSON（残余日志/警告）
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "punc_ready" => {
            // ct-punc 后台加载信号：只取 ready bool，error 丢弃（不外泄到 result）。
            let ready = v.get("ready").and_then(|b| b.as_bool()).unwrap_or(false);
            StdoutMsg::PuncReady(ready)
        }
        "punc" => {
            let text = v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let error = v
                .get("error")
                .and_then(|e| e.as_str())
                .filter(|s| !s.is_empty() && *s != "null")
                .map(|s| s.to_string());
            StdoutMsg::Punc(PuncResult { text, error })
        }
        _ => {
            // result 行（含 text/is_final/error 之一）；其余控制消息（ready/ping）→ Skip。
            if v.get("text").is_some()
                || v.get("is_final").is_some()
                || v.get("error").is_some()
            {
                let text = v
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_final = v
                    .get("is_final")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                let error = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .filter(|s| !s.is_empty() && *s != "null")
                    .map(|s| s.to_string());
                StdoutMsg::Result(AsrResult {
                    text,
                    is_final,
                    error,
                })
            } else {
                StdoutMsg::Skip
            }
        }
    }
}

// ── stdout 读取线程 ───────────────────────────────────────

/// stdout 读取循环：独占 reader，逐行经 `parse_stdout_line` 分流。
///
/// - `type=="punc"` → `punc_tx`（ct-punc 标点结果）
/// - `type=="punc_ready"` → 翻 `punc_ready` AtomicBool（ct-punc 后台懒加载就绪信号）
/// - 其余 result 行 → `result_tx`；控制消息/非 JSON → 跳过
///
/// EOF（Python 退出）时重置 `punc_ready=false`（与进程存活态对齐，便排障）+ 推 error AsrResult。
fn read_stdout_loop(
    mut reader: BufReader<std::process::ChildStdout>,
    result_tx: crossbeam_channel::Sender<AsrResult>,
    punc_tx: crossbeam_channel::Sender<PuncResult>,
    punc_ready: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF：Python 进程已关闭 stdout
                eprintln!("[PythonBridge] stdout 读取线程遇到 EOF，退出");
                punc_ready.store(false, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match parse_stdout_line(trimmed) {
                    StdoutMsg::PuncReady(ready) => {
                        punc_ready.store(ready, std::sync::atomic::Ordering::SeqCst);
                        eprintln!(
                            "[PythonBridge] ct-punc 后台加载: {}",
                            if ready {
                                "就绪（标点启用）"
                            } else {
                                "失败/降级（主字幕走原文无标点）"
                            }
                        );
                    }
                    StdoutMsg::Punc(r) => {
                        if punc_tx.send(r).is_err() {
                            break; // 接收端已断开（bridge 已析构）
                        }
                    }
                    StdoutMsg::Result(r) => {
                        if result_tx.send(r).is_err() {
                            break; // 接收端已断开（bridge 已析构）
                        }
                    }
                    StdoutMsg::Skip => {}
                }
            }
            Err(e) => {
                eprintln!("[PythonBridge] stdout 读取错误: {}", e);
                break;
            }
        }
    }

    // stdout EOF / 循环结束：Python 进程已退出（或崩溃），推送一条 error AsrResult。
    // 否则读取线程退出后主循环 try_recv 会一直返回 Empty（静默），
    // 只能等 feed_audio 触发 broken pipe 才感知，延迟报错。
    let _ = result_tx.send(AsrResult {
        text: String::new(),
        is_final: false,
        error: Some("Python 进程退出（stdout 关闭）".into()),
    });
}

// ── ready 等待 ────────────────────────────────────────────

/// 同步阻塞读 stdout 首行 ready。
///
/// 循环 `read_line`，解析 JSON：
/// - 遇到 `"models_loaded": true`（或 `"status": "ready"`）返回 `Ok(punc_loaded)`
///   （punc_loaded = ct-punc 是否就绪，决定 Rust 是否起 punc 线程）
/// - 遇到 `"status": "error"` 返回 Err(AsrError)
/// - 超时（deadline）返回 Err(LoadTimeout)
///
/// `read_line` 会阻塞直到有行；deadline 由调用者控制总时长。
fn wait_first_ready(
    reader: &mut BufReader<std::process::ChildStdout>,
    timeout_secs: u64,
) -> Result<bool, BridgeError> {
    eprintln!("[PythonBridge] 等待 Python 进程初始化（模型加载，最长 {}s）...", timeout_secs);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut line = String::new();

    loop {
        if std::time::Instant::now() > deadline {
            return Err(BridgeError::LoadTimeout(format!(
                "等待 ready 超时 ({}s)",
                timeout_secs
            )));
        }
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| BridgeError::IoError(format!("读取 ready 失败: {}", e)))?;
        if n == 0 {
            // EOF：子进程提前关闭 stdout
            return Err(BridgeError::ProcessExited(
                "Python 进程在 ready 前关闭 stdout".into(),
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 尝试解析为 JSON
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => {
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let models_loaded = v
                    .get("models_loaded")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                if models_loaded || status == "ready" {
                    let punc_loaded = v
                        .get("punc_loaded")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    eprintln!(
                        "[PythonBridge] 检测到 ready，模型已加载 (punc={})",
                        if punc_loaded { "on" } else { "off" }
                    );
                    return Ok(punc_loaded);
                }
                if status == "error" {
                    return Err(BridgeError::AsrError(format!(
                        "Python 端模型加载失败: {}",
                        trimmed
                    )));
                }
                eprintln!(
                    "[PythonBridge] ready 等待：状态={}，继续...",
                    status
                );
            }
            Err(_) => {
                // 非 JSON（日志/警告），忽略
                eprintln!(
                    "[PythonBridge] 初始日志: {}",
                    &trimmed[..trimmed.len().min(80)]
                );
            }
        }
    }
}

// ── 工具函数 ──────────────────────────────────────────────

/// 在 PATH 中查找 python 可执行文件
pub fn find_python() -> Result<String, BridgeError> {
    // 首先尝试 python3
    for candidate in &["python", "python3"] {
        if let Ok(output) = std::process::Command::new(candidate)
            .arg("--version")
            .output()
        {
            if output.status.success() {
                let version = String::from_utf8_lossy(&output.stdout);
                let version = version.trim();
                eprintln!("[PythonBridge] 找到 Python: {} ({})", candidate, version);
                return Ok(candidate.to_string());
            }
        }
    }
    Err(BridgeError::PythonNotFound(
        "未找到 Python。请安装 Python 3.10+ 并添加到 PATH。".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_stdout_line（ct-punc 懒加载信号分流）──

    #[test]
    fn parse_punc_ready_true() {
        assert!(matches!(
            parse_stdout_line(r#"{"type":"punc_ready","ready":true}"#),
            StdoutMsg::PuncReady(true)
        ));
    }

    #[test]
    fn parse_punc_ready_false_error_not_leaked() {
        // 失败信号带 error，但 PuncReady 只取 bool，error 不外泄到 result（防误触发 asr-error）
        assert!(matches!(
            parse_stdout_line(r#"{"type":"punc_ready","ready":false,"error":"boom"}"#),
            StdoutMsg::PuncReady(false)
        ));
    }

    #[test]
    fn parse_punc_response() {
        match parse_stdout_line(r#"{"type":"punc","text":"你好，世界。","error":null}"#) {
            StdoutMsg::Punc(r) => assert_eq!(r.text, "你好，世界。"),
            other => panic!("期望 Punc，得到 {:?}", other),
        }
    }

    #[test]
    fn parse_result_line() {
        match parse_stdout_line(r#"{"text":"hi","is_final":false,"error":null}"#) {
            StdoutMsg::Result(r) => assert_eq!(r.text, "hi"),
            other => panic!("期望 Result，得到 {:?}", other),
        }
    }

    #[test]
    fn parse_ready_control_skip() {
        // ready/ping 控制行（无 text/is_final/error/type=punc*）→ Skip
        assert!(matches!(
            parse_stdout_line(r#"{"status":"ready","models_loaded":true,"punc_loaded":false}"#),
            StdoutMsg::Skip
        ));
    }

    #[test]
    fn parse_non_json_skip() {
        assert!(matches!(parse_stdout_line("not json {{{"), StdoutMsg::Skip));
    }

    #[test]
    fn parse_empty_skip() {
        assert!(matches!(parse_stdout_line("   "), StdoutMsg::Skip));
    }

    #[test]
    fn stderr_log_path_format() {
        // ISSUE-5：日志路径必须含 logs\asr_ 前缀 + .log 后缀（防 typo 致静默无日志）
        let p = stderr_log_path(r"C:\Users\me\Documents\VoiceDown");
        let s = p.to_string_lossy();
        assert!(s.ends_with(".log"), "应以 .log 结尾: {}", s);
        assert!(s.contains(r"logs\asr_"), "应含 logs\\asr_ 前缀: {}", s);
    }

    #[test]
    fn drain_stderr_consumes_all_even_without_file() {
        // ISSUE-5 根因回归：日志文件 open 失败（file=None）时仍必须读尽 reader，
        // 否则 reader 提前 drop 会关闭 pipe 读端 → Python 写 stderr 报 [Errno 22] → load_models 崩溃。
        use std::io::{BufRead, BufReader, Cursor};
        let data = b"[ASR] line1\nmodelscope info\njieba debug\n";
        let mut reader = BufReader::new(Cursor::new(data.to_vec()));
        drain_stderr(&mut reader, None::<&mut std::fs::File>);
        // 所有数据被消费（reader 到 EOF，无残留）
        let mut leftover = String::new();
        assert_eq!(reader.read_line(&mut leftover).unwrap(), 0);
        assert!(leftover.is_empty());
    }

    #[test]
    fn drain_stderr_writes_to_file_when_present() {
        use std::io::{BufReader, Cursor};
        let data = b"hello\nworld\n";
        let mut reader = BufReader::new(Cursor::new(data.to_vec()));
        let mut sink = std::vec::Vec::new();
        drain_stderr(&mut reader, Some(&mut sink));
        assert_eq!(&sink, b"hello\nworld\n");
    }

}
