// useAsrState：ASR 状态机 hook（F 候选 F2 自 App.tsx 抽出）。
//
// 收拢 asrPhase 状态机（loading/ready/respawning/error/unavailable）+ 双机制兜底
//（asr-ready/asr-error 事件 + 2s 轮询）。⚠ 红线：轮询不可删——它是 asr-ready 事件不到
// 前端的兜底（commit c49958b）。hook 自包含 init（mount 即 checkAsrReady 拉首态，
// 不等 2s 轮询）。
import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import type { AsrPhase } from "../types";

export function useAsrState() {
  // ASR 状态单一事实源（ISSUE-2）：asrPhase 驱动按钮/徽标；asrReady/asrLoading 派生，避免多布尔不一致。
  const [asrPhase, setAsrPhase] = useState<AsrPhase>("loading");
  const [asrErrorMsg, setAsrErrorMsg] = useState<string>("");
  const asrReady = asrPhase === "ready";
  const asrLoading = asrPhase === "loading" || asrPhase === "respawning";

  // ASR 状态映射（ISSUE-2）：get_asr_state 返回值 → asrPhase + asrErrorMsg（ASR 专用错误，与通用 errorMsg 分离）
  const applyAsrState = useCallback((s: string) => {
    if (s === "ready") {
      setAsrPhase("ready");
      setAsrErrorMsg("");
    } else if (s === "loading" || s === "respawning") {
      setAsrPhase(s);
    } else if (s.startsWith("error:")) {
      setAsrPhase("error");
      setAsrErrorMsg("ASR 加载失败: " + s.slice(6));
    } else {
      // "unavailable"（非 asr 编译）→ 可开始纯音频捕获
      setAsrPhase("unavailable");
    }
  }, []);

  const checkAsrReady = useCallback(async () => {
    // 用 get_asr_state(loading/ready/respawning/error/unavailable) 而非 check_asr_ready
    // （仅查 python+依赖环境），正确反映「模型」加载与崩溃自愈状态。
    try {
      applyAsrState(await invoke<string>("get_asr_state"));
    } catch {
      setAsrPhase("unavailable");
    }
  }, [applyAsrState]);

  const handleRestartAsr = useCallback(async () => {
    // ISSUE-2：用户手动「重启 ASR」→ 后端从 Error 重新 respawn，轮询接管恢复。
    setAsrPhase("respawning");
    setAsrErrorMsg("");
    try {
      await invoke("restart_asr");
    } catch (e) {
      setAsrPhase("error");
      setAsrErrorMsg("重启失败: " + String(e));
    }
  }, []);

  // mount init：立即拉首态（不等 2s 轮询）。原 App 启动 effect 的 checkAsrReady() 归此。
  useEffect(() => {
    checkAsrReady();
  }, [checkAsrReady]);

  // 监听 ASR 预加载状态（asr-ready 就绪 / asr-error 失败）
  useEffect(() => {
    let unlistenReady: UnlistenFn | null = null;
    let unlistenError: UnlistenFn | null = null;
    listen("asr-ready", () => {
      console.log("[VoiceDown] asr-ready 事件已收到");
      setAsrPhase("ready");
      setAsrErrorMsg("");
    }).then((fn) => (unlistenReady = fn));
    listen<{ [key: string]: string } | string>("asr-error", (event) => {
      console.log("[VoiceDown] asr-error 事件已收到", event.payload);
      setAsrPhase("error");
      setAsrErrorMsg("ASR 加载失败: " + String(event.payload));
    }).then((fn) => (unlistenError = fn));
    // 注：asr-ready/asr-error 事件由下方常驻轮询兜底（每 2s 查 get_asr_state），
    // 避免事件丢失/时机问题导致永久卡在加载中。
    return () => {
      unlistenReady?.();
      unlistenError?.();
    };
  }, []);

  // ASR 状态轮询（ISSUE-2 常驻）：每 2s 同步后端 phase，捕获 ready↔respawning↔error 变化
  // （含 Python 崩溃后自动重生、用户重启恢复）。比事件更可靠（不丢时机）。⚠ 红线：不可删。
  useEffect(() => {
    let active = true;
    const poll = setInterval(async () => {
      try {
        const s = await invoke<string>("get_asr_state");
        if (active) applyAsrState(s);
      } catch {
        /* 忽略，下次重试 */
      }
    }, 2000);
    return () => {
      active = false;
      clearInterval(poll);
    };
  }, [applyAsrState]);

  return { asrPhase, asrErrorMsg, asrReady, asrLoading, handleRestartAsr };
}
