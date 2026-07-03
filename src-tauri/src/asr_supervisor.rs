//! ASR 崩溃自愈监管器（B 候选）。
//!
//! 合并原双轨表示：`python_bridge` 的 `BridgePhase`/`next_bridge_state`（纯函数，已测）+
//! `lib.rs` 的 `AsrLoadState`/`ensure_bridge_alive`/`respawn_loop`（运行时，0 测）→ 一个
//! `AsrSupervisor`。报告 B：同一状态机不再劈成「纯函数半 + 运行时半」靠注释人工对齐。
//!
//! **架构**：
//! - `Phase`（策略视图，不持 `Arc`，纯可测）+ `next_phase`（= 原 `next_bridge_state`，12 测试覆盖）
//! - `AsrSupervisor` 持 `Mutex<Inner { phase, bridge: Option<Arc<PythonAsrBridge>> }>`（**单锁**，
//!   invariant：`phase == Ready` ⟺ `bridge == Some`）
//! - `Spawner`（spawn）+ `Emitter`（emit asr-ready/asr-error）双 trait 注入 → 编排全可单测
//!   （产线真实现 / 测试 Fake 记录调用序列）。报告 B 的「两个 adapter 证 seam」。
//!
//! **边界**：supervisor 只管「进程崩了怎么办」状态机 + spawn/emit 编排；`PythonAsrBridge`
//! 的子进程管理（feed_audio/result_rx/stdout/shutdown）仍在 `python_bridge`。

use std::sync::{Arc, Mutex};
use crate::python_bridge::PythonAsrBridge;
use tauri::AppHandle;

/// 自动重生上限：累计失败达此值即降级 Error（= 总尝试次数，对应「限 1-2 次重试」）。
pub const MAX_RESPAWN_ATTEMPTS: u32 = 2;

// ── 纯策略层（Phase / Event / next_phase）─────────────────────

/// 监管阶段（策略视图，不持 `Arc<PythonAsrBridge>`，便于纯函数推理与单测）。
///
/// 与原 `python_bridge::BridgePhase` 的差异：`Error` 持错误信息（供 `get_asr_state`
/// 序列化给前端）。`next_phase` 不拼业务信息（超限 Error 用空串占位，编排层覆盖）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Loading,
    Ready,
    /// 重生中；attempts = 至今失败次数。
    Respawning { attempts: u32 },
    /// 加载失败 / 重生超限（等用户手动「重启 ASR」）。
    Error(String),
}

/// 驱动状态机转移的事件。
///
/// 非测试构建只用 `RespawnFail`（`respawn_loop`）；`Spawned`/`Exited`/`Restart`
/// 由 `#[cfg(test)]` 单测构造覆盖（镜像原 `python_bridge::BridgeEvent` 的 dead_code 处理）。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// spawn/respawn 成功（子进程就绪）→ Ready。
    Spawned,
    /// 子进程退出（探活 / stdout EOF）。
    Exited,
    /// 一次 respawn 尝试抛错。
    RespawnFail,
    /// 用户点击「重启 ASR」（仅 Error 态有意义）。
    Restart,
}

/// 纯函数：给定当前阶段与事件，返回下一阶段（搬运自 `python_bridge::next_bridge_state`）。
///
/// 策略：任何阶段 Spawned→Ready；Loading/Ready 上 Exited→进入重生(attempts=0)；
/// 重生中失败/再次退出→attempts+1，达 `MAX_RESPAWN_ATTEMPTS`→Error（空串占位，信息由
/// 编排层拼装）；Error+Restart→重生。无意义组合（Error+Exited、Ready+Restart）保持当前。
///
/// **注**：Error 转移返回空串（`next_phase` 是纯策略，不拼业务信息）；`AsrSupervisor`
/// 编排层在检测到 Error 转移时用真实信息覆盖（见 `respawn_loop`）。
pub fn next_phase(phase: &Phase, event: &Event) -> Phase {
    use Event::*;
    use Phase::*;
    match (phase, event) {
        (_, Spawned) => Ready,
        (Loading, Exited) | (Ready, Exited) => Respawning { attempts: 0 },
        (Respawning { attempts }, Exited) | (Respawning { attempts }, RespawnFail) => {
            bump_or_error(*attempts)
        }
        (Error(_), Restart) => Respawning { attempts: 0 },
        _ => phase.clone(),
    }
}

/// 失败计数 +1；达上限返回 Error（空串占位），否则返回 Respawning{attempts=next}。
fn bump_or_error(attempts: u32) -> Phase {
    let next = attempts + 1;
    if next >= MAX_RESPAWN_ATTEMPTS {
        Phase::Error(String::new())
    } else {
        Phase::Respawning { attempts: next }
    }
}

// ── Spawner / Emitter trait（IO 边界，注入可测）──────────────

/// spawn 一个新 `PythonAsrBridge`（产线：真子进程；测试：fake）。
pub trait Spawner: Send + Sync {
    fn spawn(&self, script: &str, save_dir: &str) -> Result<PythonAsrBridge, String>;
}

/// 产线 Spawner：调 `PythonAsrBridge::spawn`，`BridgeError`→`String`。
pub struct RealSpawner;
impl Spawner for RealSpawner {
    fn spawn(&self, script: &str, save_dir: &str) -> Result<PythonAsrBridge, String> {
        PythonAsrBridge::spawn(script, save_dir).map_err(|e| format!("{}", e))
    }
}

/// 通知前端 ASR 就绪/出错（产线：`app_handle.emit`；测试：fake 记录）。
pub trait Emitter: Send + Sync {
    fn emit_ready(&self);
    fn emit_error(&self, msg: &str);
}

/// 产线 Emitter：`asr-ready` / `asr-error` 事件（前端轮询 `get_asr_state` 双机制兜底）。
pub struct RealEmitter(AppHandle);
impl RealEmitter {
    /// 构造产线 Emitter（持 `AppHandle`，经 `tauri::Emitter` 发事件）。
    pub fn new(app: AppHandle) -> Self {
        Self(app)
    }
}
impl Emitter for RealEmitter {
    fn emit_ready(&self) {
        let _ = tauri::Emitter::emit(&self.0, "asr-ready", ());
    }
    fn emit_error(&self, msg: &str) {
        let _ = tauri::Emitter::emit(&self.0, "asr-error", msg);
    }
}

// ── AsrSupervisor（运行时，持 Phase + bridge）──────────────────

/// supervisor 内部可变状态（单锁保护，保 `phase == Ready ⟺ bridge == Some` invariant）。
struct Inner {
    phase: Phase,
    /// Ready 时持有 bridge；其他态 None。旧 bridge 的 Drop（shutdown 等待子进程）在锁外执行。
    bridge: Option<Arc<PythonAsrBridge>>,
}

/// ASR 崩溃自愈监管器（应用级单例，内部 `Mutex` 保护；外部 `Arc` 共享 + arb-self 线程 kick）。
pub struct AsrSupervisor {
    inner: Mutex<Inner>,
    /// `asr_server.py` 路径；None = 脚本未找到（spawn_initial/respawn 直接 Error）。
    script: Option<String>,
    /// stderr 落盘目录（`logs/asr_<ts>.log`，ISSUE-5）。
    save_dir: String,
    spawner: Box<dyn Spawner>,
    emitter: Box<dyn Emitter>,
}

impl AsrSupervisor {
    /// 构造监管器（phase=Loading）。返回 `Arc` 供 `ensure_alive`/`restart` 的 arb-self 线程 kick。
    pub fn new(
        script: Option<String>,
        save_dir: String,
        spawner: Box<dyn Spawner>,
        emitter: Box<dyn Emitter>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                phase: Phase::Loading,
                bridge: None,
            }),
            script,
            save_dir,
            spawner,
            emitter,
        })
    }

    /// 当前阶段（`get_asr_state` 序列化用）。
    pub fn phase(&self) -> Phase {
        self.inner.lock().unwrap().phase.clone()
    }

    /// 取 Ready bridge（`start_capture` 复用单例用）。非 Ready 态返回 None。
    pub fn ready_bridge(&self) -> Option<Arc<PythonAsrBridge>> {
        let inner = self.inner.lock().unwrap();
        if matches!(inner.phase, Phase::Ready) {
            inner.bridge.clone()
        } else {
            None
        }
    }

    /// 初始加载（`preload_asr` 后台线程同步调）：spawn → Ready(emit ready) / Error(emit error)。
    /// script 缺失 → Error("asr_server.py 未找到")。
    pub fn spawn_initial(&self) {
        let script = match self.script_or_error() {
            Ok(s) => s,
            Err(msg) => {
                self.set_error_and_emit(&msg);
                eprintln!("[VoiceDown] asr_server.py 未找到，ASR 不可用");
                return;
            }
        };
        match self.spawner.spawn(script, &self.save_dir) {
            Ok(bridge) => {
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.phase = Phase::Ready;
                    inner.bridge = Some(Arc::new(bridge));
                }
                self.emitter.emit_ready();
                eprintln!("[VoiceDown] ASR bridge 预加载完成，就绪");
            }
            Err(e) => {
                let msg = format!("ASR 加载失败: {}", e);
                self.set_error_and_emit(&msg);
                eprintln!("[VoiceDown] ASR bridge 预加载失败: {}", e);
            }
        }
    }

    /// 探活 Ready bridge；若已死，CAS Ready→Respawning{0}（旧 Arc 锁外 drop）+ 后台重生。
    /// 返回 true = 刚触发重生（调用方应拒绝当前操作/返回 respawning）。
    pub fn ensure_alive(self: &Arc<Self>) -> bool {
        let dead_arc = self.take_dead_bridge();
        if let Some(_old) = dead_arc {
            drop(_old); // 旧 bridge Drop（shutdown）锁外，避免持锁等待
            eprintln!("[VoiceDown] 检测到 ASR 进程退出，触发自动重生");
            self.kick_respawn();
            true
        } else {
            false
        }
    }

    /// CAS：若 Ready 且 bridge 已死，置 Respawning{0} 并取出旧 Arc（调用方锁外 drop）。
    /// 拆为独立方法便于强调「锁外 drop」语义；探活本身需 Ready+真 bridge（由 e2e 覆盖）。
    fn take_dead_bridge(&self) -> Option<Arc<PythonAsrBridge>> {
        let mut inner = self.inner.lock().unwrap();
        if let Phase::Ready = inner.phase {
            if let Some(ref arc) = inner.bridge {
                if !arc.is_alive() {
                    let old = inner.bridge.take();
                    inner.phase = Phase::Respawning { attempts: 0 };
                    return old;
                }
            }
        }
        None
    }

    /// 用户手动重启（仅 Error 态生效 → Respawning{0} + 后台重生）。返回 true=已触发。
    pub fn restart(self: &Arc<Self>) -> bool {
        if self.try_restart_cas() {
            self.kick_respawn();
            true
        } else {
            false
        }
    }

    /// CAS：若 Error，置 Respawning{0}。返回是否触发（拆出便于单测，不 kick）。
    fn try_restart_cas(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.phase, Phase::Error(_)) {
            inner.phase = Phase::Respawning { attempts: 0 };
            true
        } else {
            false
        }
    }

    /// 启动后台重生线程（spawn 慢 10~90s，绝不阻塞 IPC/主线程）。
    fn kick_respawn(self: &Arc<Self>) {
        let arc = Arc::clone(self);
        std::thread::Builder::new()
            .name("asr-respawn".into())
            .spawn(move || arc.respawn_loop())
            .expect("spawn asr-respawn");
    }

    /// 重生循环：读 attempts → spawn → 成功 Ready(emit ready) / 失败按状态机计数，超限 Error(emit error)。
    /// 状态被他人改离 Respawning（app 退出等）则退出。
    fn respawn_loop(&self) {
        let script = match self.script_or_error() {
            Ok(s) => s,
            Err(msg) => {
                self.set_error_and_emit(&msg);
                return;
            }
        };
        loop {
            let attempts = {
                let inner = self.inner.lock().unwrap();
                match inner.phase {
                    Phase::Respawning { attempts } => attempts,
                    _ => return,
                }
            };
            eprintln!("[VoiceDown] ASR 重生尝试 (attempts={})", attempts);
            match self.spawner.spawn(script, &self.save_dir) {
                Ok(bridge) => {
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.phase = Phase::Ready;
                        inner.bridge = Some(Arc::new(bridge));
                    }
                    self.emitter.emit_ready();
                    eprintln!("[VoiceDown] ASR 重生成功，bridge 就绪");
                    return;
                }
                Err(e) => {
                    eprintln!("[VoiceDown] ASR 重生失败: {}", e);
                    let new = next_phase(&Phase::Respawning { attempts }, &Event::RespawnFail);
                    match new {
                        Phase::Respawning { attempts: a } => {
                            let mut inner = self.inner.lock().unwrap();
                            if matches!(inner.phase, Phase::Respawning { .. }) {
                                inner.phase = Phase::Respawning { attempts: a };
                            }
                            continue;
                        }
                        Phase::Error(_) => {
                            // next_phase 返回空串占位；编排层拼真实信息
                            let msg = format!(
                                "ASR 多次重启失败（已达上限 {} 次）",
                                MAX_RESPAWN_ATTEMPTS
                            );
                            self.set_error_and_emit(&format!("ASR 重启失败: {}", msg));
                            return;
                        }
                        _ => return, // 防御：状态机不应返回其它分支
                    }
                }
            }
        }
    }

    /// script 存在则返回其引用；缺失则返回要 set 的 Error 信息。
    fn script_or_error(&self) -> Result<&str, String> {
        match &self.script {
            Some(s) => Ok(s.as_str()),
            None => Err("asr_server.py 未找到".into()),
        }
    }

    /// 置 Error{msg} + 清 bridge + emit_error（锁内 set / 锁外 emit）。
    fn set_error_and_emit(&self, msg: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.phase = Phase::Error(msg.to_string());
            inner.bridge = None;
        }
        self.emitter.emit_error(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── next_phase（搬运自 python_bridge，12 测试；Error 断言改 matches!）──

    #[test]
    fn loading_spawned_to_ready() {
        assert_eq!(next_phase(&Phase::Loading, &Event::Spawned), Phase::Ready);
    }

    #[test]
    fn ready_spawned_stays_ready() {
        assert_eq!(next_phase(&Phase::Ready, &Event::Spawned), Phase::Ready);
    }

    #[test]
    fn ready_exited_enters_respawn() {
        // Python 进程崩溃（idle 或捕获中探活到）：Ready → Respawning{0}
        assert_eq!(
            next_phase(&Phase::Ready, &Event::Exited),
            Phase::Respawning { attempts: 0 }
        );
    }

    #[test]
    fn loading_exited_enters_respawn() {
        // 加载阶段就退出（极端）：也进入重生而非永久 Loading。
        assert_eq!(
            next_phase(&Phase::Loading, &Event::Exited),
            Phase::Respawning { attempts: 0 }
        );
    }

    #[test]
    fn respawning_spawned_to_ready() {
        assert_eq!(
            next_phase(&Phase::Respawning { attempts: 1 }, &Event::Spawned),
            Phase::Ready
        );
    }

    #[test]
    fn respawning_first_fail_retries() {
        // 第 1 次重生失败（attempts 0→1，未达上限）→ 继续重生
        assert_eq!(
            next_phase(&Phase::Respawning { attempts: 0 }, &Event::RespawnFail),
            Phase::Respawning { attempts: 1 }
        );
    }

    #[test]
    fn respawning_second_fail_gives_up() {
        // 第 2 次重生失败（attempts 1→2，达 MAX=2）→ 降级 Error（空串占位）
        let r = next_phase(&Phase::Respawning { attempts: 1 }, &Event::RespawnFail);
        assert!(matches!(r, Phase::Error(_)), "应 Error，得到 {:?}", r);
    }

    #[test]
    fn respawning_exit_counts_as_fail() {
        // 重生后又退出：与 RespawnFail 同等计数（新生子进程立即死）。
        assert_eq!(
            next_phase(&Phase::Respawning { attempts: 0 }, &Event::Exited),
            Phase::Respawning { attempts: 1 }
        );
    }

    #[test]
    fn error_restart_reenters_respawn() {
        // 用户点「重启 ASR」：Error → Respawning{0}（重新给重生机会）
        assert_eq!(
            next_phase(&Phase::Error("e".into()), &Event::Restart),
            Phase::Respawning { attempts: 0 }
        );
    }

    #[test]
    fn error_exited_stays_error() {
        // 已降级 Error 后再探到退出：无意义，保持 Error（不重复计数）。
        let r = next_phase(&Phase::Error("e".into()), &Event::Exited);
        assert!(matches!(r, Phase::Error(_)), "应保持 Error，得到 {:?}", r);
    }

    #[test]
    fn ready_restart_no_op() {
        // Ready 时「重启」无意义：保持 Ready。
        assert_eq!(next_phase(&Phase::Ready, &Event::Restart), Phase::Ready);
    }

    #[test]
    fn max_attempts_is_two_total_tries() {
        // 端到端策略校验：Ready 崩溃 → 两次 respawn 全失败 → Error（共 2 次尝试）。
        let s0 = Phase::Ready;
        let s1 = next_phase(&s0, &Event::Exited);
        assert_eq!(s1, Phase::Respawning { attempts: 0 });
        let s2 = next_phase(&s1, &Event::RespawnFail);
        assert_eq!(s2, Phase::Respawning { attempts: 1 });
        let s3 = next_phase(&s2, &Event::RespawnFail);
        assert!(matches!(s3, Phase::Error(_)), "两次失败应 Error，得到 {:?}", s3);
    }

    // ── 编排测试（FakeSpawner 总失败 + FakeEmitter 记录）──
    // 成功路径（Ok→Ready+存 bridge）需真 PythonAsrBridge（字段全持真子进程，无法 cheap fake），
    // 由 next_phase 的 Spawned→Ready 纯转移 + e2e 覆盖；此处只测失败/状态机编排。

    /// 总返 Err 的 Spawner（驱动失败/超限编排）。
    struct FakeSpawner;
    impl Spawner for FakeSpawner {
        fn spawn(&self, _script: &str, _save_dir: &str) -> Result<PythonAsrBridge, String> {
            Err("fake spawn fail".into())
        }
    }

    /// 记录 emit 调用的 Emitter（Arc<Mutex> 共享，Clone 后测试仍可读）。
    #[derive(Clone)]
    struct FakeEmitter {
        ready_calls: Arc<Mutex<u32>>,
        errors: Arc<Mutex<Vec<String>>>,
    }
    impl FakeEmitter {
        fn new() -> Self {
            Self {
                ready_calls: Arc::new(Mutex::new(0)),
                errors: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }
    impl Emitter for FakeEmitter {
        fn emit_ready(&self) {
            *self.ready_calls.lock().unwrap() += 1;
        }
        fn emit_error(&self, msg: &str) {
            self.errors.lock().unwrap().push(msg.to_string());
        }
    }

    /// 构造测试用 supervisor（FakeSpawner 总失败 + Some(script)）。
    fn make_sup() -> (Arc<AsrSupervisor>, FakeEmitter) {
        let emitter = FakeEmitter::new();
        let sup = AsrSupervisor::new(
            Some("script".into()),
            "dir".into(),
            Box::new(FakeSpawner),
            Box::new(emitter.clone()),
        );
        (sup, emitter)
    }

    #[test]
    fn spawn_initial_fail_sets_error_and_emits() {
        // FakeSpawner 总失败：spawn_initial → Error("ASR 加载失败...") + emit_error
        let (sup, emitter) = make_sup();
        sup.spawn_initial();
        match sup.phase() {
            Phase::Error(msg) => assert!(msg.contains("ASR 加载失败"), "msg={}", msg),
            other => panic!("失败应 Error，得到 {:?}", other),
        }
        let errs = emitter.errors.lock().unwrap();
        assert_eq!(errs.len(), 1, "应 emit 一次 error: {:?}", errs);
        assert!(errs[0].contains("ASR 加载失败"), "{}", errs[0]);
    }

    #[test]
    fn spawn_initial_no_script_sets_error() {
        // script 缺失：spawn_initial → Error("asr_server.py 未找到")（不调 spawner）
        let emitter = FakeEmitter::new();
        let sup = AsrSupervisor::new(
            None,
            "dir".into(),
            Box::new(FakeSpawner),
            Box::new(emitter.clone()),
        );
        sup.spawn_initial();
        match sup.phase() {
            Phase::Error(msg) => assert!(msg.contains("asr_server.py"), "msg={}", msg),
            other => panic!("script 缺失应 Error，得到 {:?}", other),
        }
        assert_eq!(emitter.errors.lock().unwrap().len(), 1);
    }

    #[test]
    fn respawn_loop_gives_up_after_max() {
        // FakeSpawner 总失败：Error → restart_cas → Respawning{0} → respawn_loop → 2 次失败 → Error("已达上限")
        let (sup, emitter) = make_sup();
        sup.spawn_initial(); // → Error("ASR 加载失败...")
        assert!(sup.try_restart_cas()); // Error → Respawning{0}
        assert_eq!(sup.phase(), Phase::Respawning { attempts: 0 });
        sup.respawn_loop(); // 同步调（绕过 kick 线程）：2 次 spawn 失败 → Error
        match sup.phase() {
            Phase::Error(msg) => assert!(msg.contains("已达上限"), "msg={}", msg),
            other => panic!("超限应 Error，得到 {:?}", other),
        }
        // spawn_initial 的 emit_error + respawn_loop 的 emit_error
        let errs = emitter.errors.lock().unwrap();
        assert!(
            errs.iter().any(|e| e.contains("已达上限")),
            "应 emit 超限 error: {:?}",
            errs
        );
    }

    #[test]
    fn restart_cas_only_from_error() {
        // try_restart_cas：Loading 态不触发；Error 态触发 → Respawning{0}
        let (sup, _) = make_sup();
        assert!(!sup.try_restart_cas(), "Loading 态不应触发 restart");
        assert_eq!(sup.phase(), Phase::Loading);
        sup.spawn_initial(); // → Error
        assert!(sup.try_restart_cas(), "Error 态应触发 restart");
        assert_eq!(sup.phase(), Phase::Respawning { attempts: 0 });
    }

    #[test]
    fn ready_bridge_none_when_not_ready() {
        // 非 Ready 态（Loading/Error）ready_bridge 返 None（start_capture 据此拒绝）
        let (sup, _) = make_sup();
        assert!(sup.ready_bridge().is_none(), "Loading 态应无 bridge");
        sup.spawn_initial(); // → Error
        assert!(sup.ready_bridge().is_none(), "Error 态应无 bridge");
    }

    #[test]
    fn initial_phase_is_loading() {
        let (sup, _) = make_sup();
        assert_eq!(sup.phase(), Phase::Loading);
    }
}
