// useTranscriptionStreams：优化/定稿数据流 hook（F 候选 F3 自 App.tsx 抽出）。
//
// 收拢纯推送型 listen（asr-optimize/asr-optimize-warn/asr-finalize/asr-finalize-warn）
// → 优化文本 + 离线定稿状态 + 草稿/定稿视图切换。定稿触发（handleFinalize）+ 全流重置
//（resetTranscriptionStreams，handleStart 用）也归此（定稿操作与定稿 state 内聚）。
// asr-transcription 动态监听 / asr-warning / export-* / 捕获轮询归 useCaptureLifecycle
//（F4，capture 语义）。
import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import type { OptimizeEvent, FinalizeEvent } from "../types";

export function useTranscriptionStreams() {
  const [optimizedText, setOptimizedText] = useState<string>("");
  const [optimizeWarn, setOptimizeWarn] = useState<string>("");
  // 离线定稿：结构化 markdown 产物 + 防重入 + 错误（草稿/定稿切换留 ISSUE-2）
  const [finalText, setFinalText] = useState<string>("");
  const [finalizing, setFinalizing] = useState<boolean>(false);
  const [finalizeError, setFinalizeError] = useState<string>("");
  // 草稿/定稿视图切换（定稿完成自动切 final；开始捕获重置 draft）
  const [viewMode, setViewMode] = useState<"draft" | "final">("draft");

  // 监听 F-14 文本优化事件
  useEffect(() => {
    let unOpt: UnlistenFn | null = null;
    let unWarn: UnlistenFn | null = null;
    listen<OptimizeEvent>("asr-optimize", (event) => {
      setOptimizedText(event.payload.full_optimized);
    }).then((fn) => (unOpt = fn));
    listen<string>("asr-optimize-warn", (event) => {
      setOptimizeWarn(String(event.payload));
    }).then((fn) => (unWarn = fn));
    return () => {
      unOpt?.();
      unWarn?.();
    };
  }, []);

  // 离线定稿事件（asr-finalize 成功 / asr-finalize-warn 失败）
  useEffect(() => {
    let unFin: UnlistenFn | null = null;
    let unFinWarn: UnlistenFn | null = null;
    listen<FinalizeEvent>("asr-finalize", (event) => {
      setFinalText(event.payload.final_text);
      setFinalizing(false);
      setFinalizeError("");
      setViewMode("final");
    }).then((fn) => (unFin = fn));
    listen<string>("asr-finalize-warn", (event) => {
      setFinalizing(false);
      setFinalizeError(String(event.payload));
    }).then((fn) => (unFinWarn = fn));
    return () => {
      unFin?.();
      unFinWarn?.();
    };
  }, []);

  // 重置全部流状态（handleStart 开始新捕获时调）
  const resetTranscriptionStreams = useCallback(() => {
    setOptimizedText("");
    setOptimizeWarn("");
    setFinalText("");
    setFinalizing(false);
    setFinalizeError("");
    setViewMode("draft");
  }, []);

  // 离线定稿：触发后端单次整篇 LLM 调用（fire-and-forget，结果走 asr-finalize 事件）
  const handleFinalize = useCallback(async () => {
    setFinalizing(true);
    setFinalizeError("");
    try {
      await invoke("finalize_document");
    } catch (e) {
      setFinalizing(false);
      setFinalizeError(String(e));
    }
  }, []);

  return {
    optimizedText,
    optimizeWarn,
    finalText,
    finalizing,
    finalizeError,
    viewMode,
    setViewMode,
    resetTranscriptionStreams,
    handleFinalize,
  };
}
