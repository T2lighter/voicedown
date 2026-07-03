// useCaptureLifecycle：捕获生命周期 hook（F 候选 F4 自 App.tsx 抽出，F 伞收尾）。
//
// 收拢 status 状态机（idle/capturing/stopping/exporting）+ handleStart/handleStop +
// asr-transcription 动态监听（建:handleStart / 拆:export-done·stop失败·卸载）+ asr-warning
// 丢帧告警 + export-progress/export-done 导出流 + 捕获状态轮询。export-done 跨域（setStatus/
// 拆 unlisten + setExportPhase/setAudioSrc）归此（capture 收尾语义）。
//
// 参数化：asrReady（F2 useAsrState）/ selectedPid（App 窗口选择）/ setErrorMsg（App 通用错误，
// 与 fetchWindows 共享）/ resetTranscriptionStreams（F3 优化定稿流重置）。errorMsg/fetchWindows
// 留 App（非 capture 域）。
import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { convertFileSrc } from "@tauri-apps/api/core";
import type {
  CaptureStatus,
  CaptureStatusInfo,
  TranscriptionEvent,
  ExportProgress,
  ExportDone,
} from "../types";

interface UseCaptureLifecycleArgs {
  asrReady: boolean;
  selectedPid: number | null;
  setErrorMsg: (s: string) => void;
  resetTranscriptionStreams: () => void;
}

export function useCaptureLifecycle({
  asrReady,
  selectedPid,
  setErrorMsg,
  resetTranscriptionStreams,
}: UseCaptureLifecycleArgs) {
  const [status, setStatus] = useState<CaptureStatus>("idle");
  const [stats, setStats] = useState<CaptureStatusInfo | null>(null);
  const [audioSrc, setAudioSrc] = useState<string | null>(null);
  const [audioLabel, setAudioLabel] = useState<string>("");
  const [dropWarn, setDropWarn] = useState<boolean>(false);
  const [transcription, setTranscription] = useState<string>("");
  // ISSUE-2：导出阶段（export-progress phase），驱动「导出中」分阶段文案
  const [exportPhase, setExportPhase] = useState<string>("");
  // asr-transcription 动态监听句柄（仅 asrReady 且捕获时存在；4 处建/拆见上方注释）
  const unlistenRef = useRef<UnlistenFn | null>(null);

  // ISSUE-5：丢帧告警（audio_capture 背压丢块 → asr-warning 限流推送，仅捕获期间显示）
  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    listen<number>("asr-warning", () => {
      setDropWarn(true);
    }).then((fn) => (unlisten = fn));
    return () => {
      unlisten?.();
    };
  }, []);

  // ISSUE-2：导出进度 + 完成（后台 finalize 线程 emit；长驻监听，仅 exporting 态有意义）。
  // export-done 在此 unlisten asr-transcription（drain 末段已上屏，此时方可拆除）。
  useEffect(() => {
    let unProg: UnlistenFn | null = null;
    let unDone: UnlistenFn | null = null;
    listen<ExportProgress>("export-progress", (event) => {
      setExportPhase(event.payload.phase);
    }).then((fn) => (unProg = fn));
    listen<ExportDone>("export-done", (event) => {
      const d = event.payload;
      setExportPhase("");
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }
      setStatus("idle");
      setDropWarn(false);
      // 先清播放面板：无 wav_path（未启用 / 无音频 / 失败）则不显示，不留上次录制
      setAudioSrc(null);
      setAudioLabel("");
      if (d.error) {
        setErrorMsg("导出失败: " + d.error);
      } else if (d.skipped) {
        setErrorMsg(d.skipped);
      } else {
        setErrorMsg("");
      }
      if (d.wav_path) {
        try {
          setAudioSrc(convertFileSrc(d.wav_path));
          setAudioLabel(
            `${d.wav_path.split("\\").pop()} (${d.duration_secs.toFixed(1)}秒)`
          );
        } catch {
          setErrorMsg("播放器初始化失败，请手动打开文件: " + d.wav_path);
        }
      }
    }).then((fn) => (unDone = fn));
    return () => {
      unProg?.();
      unDone?.();
    };
  }, [setErrorMsg]);

  // 捕获状态轮询
  useEffect(() => {
    if (status !== "capturing") return;
    const interval = setInterval(async () => {
      try {
        const s: CaptureStatusInfo = await invoke("get_capture_status");
        setStats(s);
      } catch (e) {
        console.error(e);
      }
    }, 1000);
    return () => clearInterval(interval);
  }, [status]);

  const handleStart = useCallback(async () => {
    // 目标 PID
    const targetPid = selectedPid;

    if (targetPid === null) {
      setErrorMsg("请先选择目标窗口");
      return;
    }
    try {
      setStatus("capturing");
      setErrorMsg("");
      setStats(null);
      setTranscription("");
      resetTranscriptionStreams();
      setDropWarn(false);
      setAudioSrc(null);
      setAudioLabel("");

      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }

      if (asrReady) {
        try {
          const unlisten = await listen<TranscriptionEvent>(
            "asr-transcription",
            (event) => {
              setTranscription(event.payload.full_text);
            }
          );
          unlistenRef.current = unlisten;
        } catch (e) {
          console.error("监听 ASR 事件失败:", e);
        }
      }

      // 窗口模式：include_tree=true 捕获子进程（Chrome 渲染器）
      await invoke("start_capture", {
        pid: targetPid,
      });
    } catch (e) {
      setStatus("idle");
      setErrorMsg("启动失败: " + String(e));
    }
  }, [selectedPid, asrReady, setErrorMsg, resetTranscriptionStreams]);

  const handleStop = useCallback(async () => {
    // ISSUE-2：stop_capture 立刻返回 ack（不再 await drain，那是卡顿根因）；drain + 存盘在
    // 后台 finalize 线程，靠 export-progress / export-done 驱动 UI。asr-transcription 不在此
    // unlisten——drain 末段事件还要上屏，unlisten 挪到 export-done 监听里。
    try {
      setStatus("stopping");
      setErrorMsg("");
      await invoke("stop_capture");
      setStatus("exporting");
    } catch (e) {
      setStatus("idle");
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }
      setErrorMsg("停止失败: " + String(e));
    }
  }, [setErrorMsg]);

  // 清理监听器（卸载时拆 asr-transcription）
  useEffect(() => {
    return () => {
      if (unlistenRef.current) {
        unlistenRef.current();
      }
    };
  }, []);

  return {
    status,
    stats,
    audioSrc,
    audioLabel,
    dropWarn,
    exportPhase,
    transcription,
    handleStart,
    handleStop,
  };
}
