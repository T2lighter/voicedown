import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import ReactMarkdown from "react-markdown";
import "./App.css";
import type { WindowInfo, LlmConfig } from "./types";
import { WindowDropdown } from "./components/WindowDropdown";
import { SettingsModal } from "./components/SettingsModal";
import { DictionaryModal } from "./components/DictionaryModal";
import { ExportSettingsModal } from "./components/ExportSettingsModal";
import { useAsrState } from "./hooks/useAsrState";
import { useTranscriptionStreams } from "./hooks/useTranscriptionStreams";
import { useCaptureLifecycle } from "./hooks/useCaptureLifecycle";

// 按句末标点做视觉换行（纯可读性，非语义分段）。
// 先压平后端 OverlapRewriter/prompt 带来的段落换行（\n / \n\n）→ 连续文本，
// 再按句末标点插单 \n。行内折行由 .output-content pre 的 pre-wrap + break-all 兜底。
function breakBySentence(text: string): string {
  return text
    .replace(/\s*\n\s*/g, "")
    .replace(/([。！？!?]+)/g, "$1\n");
}

function App() {
  const [errorMsg, setErrorMsg] = useState<string>("");
  const [windows, setWindows] = useState<WindowInfo[]>([]);
  const [selectedPid, setSelectedPid] = useState<number | null>(null);
  const [loadingWindows, setLoadingWindows] = useState(false);
  // ASR 状态机（F2 抽 useAsrState：asrPhase/asrErrorMsg/asrReady/asrLoading/handleRestartAsr）
  const { asrPhase, asrErrorMsg, asrReady, asrLoading, handleRestartAsr } =
    useAsrState();
  // 优化/定稿数据流（F3 抽 useTranscriptionStreams）
  const {
    optimizedText,
    optimizeWarn,
    finalText,
    finalizing,
    finalizeError,
    viewMode,
    setViewMode,
    resetTranscriptionStreams,
    handleFinalize,
  } = useTranscriptionStreams();
  // 捕获生命周期（F4 抽 useCaptureLifecycle：status 状态机 + start/stop + 转录/导出/告警流）
  const {
    status,
    stats,
    audioSrc,
    audioLabel,
    dropWarn,
    exportPhase,
    transcription,
    handleStart,
    handleStop,
  } = useCaptureLifecycle({
    asrReady,
    selectedPid,
    setErrorMsg,
    resetTranscriptionStreams,
  });
  const [llmConfig, setLlmConfig] = useState<LlmConfig | null>(null);
  const [showSettings, setShowSettings] = useState<boolean>(false);
  const [showDict, setShowDict] = useState<boolean>(false);
  const [showExport, setShowExport] = useState<boolean>(false);
  const audioRef = useRef<HTMLAudioElement>(null);
  const outputRef = useRef<HTMLDivElement>(null);

  // 启动时拉取窗口列表 & 拉取优化配置（ASR 状态由 useAsrState 自管 init）
  useEffect(() => {
    fetchWindows();
    invoke<LlmConfig>("get_llm_config")
      .then(setLlmConfig)
      .catch(() => setLlmConfig(null));
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // 自动滚动到输出底部
  useEffect(() => {
    if (outputRef.current) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [transcription, optimizedText, stats]);

  const fetchWindows = useCallback(async () => {
    setLoadingWindows(true);
    try {
      const w: WindowInfo[] = await invoke("list_windows");
      setWindows(w);
    } catch (e) {
      setErrorMsg("获取窗口列表失败: " + String(e));
    }
    setLoadingWindows(false);
  }, []);

  const saveConfig = useCallback(async (cfg: LlmConfig) => {
    try {
      await invoke("set_llm_config", { config: cfg });
    } catch (e) {
      setErrorMsg("保存配置失败: " + String(e));
    }
  }, []);

  const optimizeEnabled = !!llmConfig?.enabled;
  const statusText =
    status === "idle"
      ? "就绪"
      : status === "capturing"
      ? "捕获中"
      : status === "exporting"
      ? "导出中"
      : "停止中";
  // 时长格式化：秒 → m:ss
  const formatDuration = (secs: number) => {
    const m = Math.floor(secs / 60);
    const s = Math.floor(secs % 60);
    return `${m}:${s.toString().padStart(2, "0")}`;
  };
  // ISSUE-2：导出阶段文案（drain / 写音频 / 写文本）
  const exportPhaseLabel = (phase: string) =>
    phase === "draining"
      ? "收尾识别中…"
      : phase === "writing-audio"
      ? "写入音频…"
      : phase === "writing-text"
      ? "写入文本…"
      : "导出中…";
  const dotClass =
    status === "capturing"
      ? "active"
      : status === "stopping"
      ? "stopping"
      : status === "exporting"
      ? "exporting"
      : "idle";

  return (
    <div className="app-container">
      <header className="app-header">
        <h1>VoiceDown</h1>
        {asrReady && <span className="asr-badge">应用级音频捕获</span>}
        {dropWarn && status === "capturing" && (
          <span className="asr-badge" style={{ color: "#f59e0b" }}>
            ⚠ 检测到丢帧，转录可能不完整
          </span>
        )}
        <div className="spacer" />
        <button
          className="icon-btn"
          onClick={() => setShowDict(true)}
          disabled={status !== "idle"}
          title="词典设置"
        >
          📖
        </button>
        <button
          className="icon-btn"
          onClick={() => setShowSettings(true)}
          disabled={status !== "idle"}
          title="优化设置"
        >
          ⚙
        </button>
        <button
          className="icon-btn"
          onClick={() => setShowExport(true)}
          disabled={status !== "idle"}
          title="导出设置"
        >
          📤
        </button>
      </header>

      <main className="app-main">
        {/* 窗口选择 */}
        <div className="window-selector card">
          <WindowDropdown
            windows={windows}
            selectedPid={selectedPid}
            onSelect={setSelectedPid}
            disabled={status !== "idle"}
          />
          <button
            className="btn btn-ghost"
            onClick={fetchWindows}
            disabled={status !== "idle" || loadingWindows}
          >
            {loadingWindows ? "刷新中…" : "刷新"}
          </button>
        </div>

        {/* 双栏输出：原始转录 | 优化文本 */}
        <div className={`output-grid ${!optimizeEnabled ? "single" : ""}`}>
          <div className="output-panel">
            <div className="output-header">原始转录 (ASR)</div>
            <div className="output-content" ref={outputRef}>
              {transcription ? (
                <pre>{breakBySentence(transcription)}</pre>
              ) : (
                <div className="placeholder">
                  开始捕获后，转录文本将在此实时显示。
                </div>
              )}
            </div>
          </div>
          {optimizeEnabled && (
            <div className="output-panel optimized">
              <div className="output-header">
                <span className="output-header-title">
                  <span className="tag">✨</span>
                  优化文本 (LLM)
                </span>
                <div className="output-header-actions">
                  {(finalText || finalizing) && (
                    <div className="segmented view-toggle">
                      <button
                        className={viewMode === "draft" ? "active" : ""}
                        onClick={() => setViewMode("draft")}
                      >
                        草稿
                      </button>
                      <button
                        className={viewMode === "final" ? "active" : ""}
                        onClick={() => setViewMode("final")}
                        disabled={finalizing || !finalText}
                      >
                        定稿
                      </button>
                    </div>
                  )}
                  <button
                    className="btn btn-finalize"
                    onClick={handleFinalize}
                    disabled={status !== "idle" || finalizing || (!optimizedText && !transcription)}
                    title="整篇结构化定稿"
                  >
                    {finalizing ? "定稿中…" : "✨ 定稿"}
                  </button>
                </div>
              </div>
              <div className="output-content">
                {viewMode === "final" ? (
                  finalizing ? (
                    <div className="placeholder">⏳ 整篇定稿中，预计 30–60 秒…</div>
                  ) : finalText ? (
                    <div className="final-doc">
                      <ReactMarkdown>{finalText}</ReactMarkdown>
                    </div>
                  ) : (
                    <div className="placeholder">点击「✨ 定稿」生成结构化文档。</div>
                  )
                ) : optimizedText ? (
                  <pre>{breakBySentence(optimizedText)}</pre>
                ) : (
                  <div className="placeholder">
                    开始捕获后，LLM 优化文本将在此逐批显示。
                  </div>
                )}
                {optimizeWarn && viewMode === "draft" && (
                  <div className="hint warn">优化降级: {optimizeWarn}</div>
                )}
                {finalizeError && (
                  <div className="hint warn">定稿失败: {finalizeError}</div>
                )}
              </div>
            </div>
          )}
        </div>

        {/* 控制条：按钮 + 状态/已捕获/模式 */}
        <div className="control-bar card">
          {status === "idle" ? (
            asrPhase === "error" ? (
              <button className="btn btn-start" onClick={handleRestartAsr}>
                重启 ASR
              </button>
            ) : (
              <button
                className="btn btn-start"
                onClick={handleStart}
                disabled={asrLoading}
              >
                {asrPhase === "respawning"
                  ? "ASR 重启中…"
                  : asrLoading
                  ? "模型加载中…"
                  : "开始捕获"}
              </button>
            )
          ) : status === "exporting" ? (
            // ISSUE-2：导出期——禁用按钮 + 分阶段文案（Start 也因此不渲染 → 禁开始）
            <button className="btn btn-stop" disabled>
              {exportPhaseLabel(exportPhase)}
            </button>
          ) : (
            <button
              className="btn btn-stop"
              onClick={handleStop}
              disabled={status === "stopping"}
            >
              {status === "stopping" ? "停止中…" : "停止捕获"}
            </button>
          )}

          <div className="capture-meta">
            {/* 状态 */}
            <span className="meta-item meta-state">
              <span className={`status-dot ${dotClass}`} />
              <span className="meta-label">状态</span>
              <span className="meta-value">{statusText}</span>
              {stats?.asr_active && <span className="meta-asr">+ ASR</span>}
            </span>

            {/* 已捕获（仅捕获中显示） */}
            {status === "capturing" && stats && (
              <span className="meta-item">
                <span className="meta-label">已捕获</span>
                <span className="meta-value">{formatDuration(stats.duration_secs)}</span>
                <span className="meta-sub">{stats.samples.toLocaleString()} 样本</span>
              </span>
            )}

            {/* 模式 */}
            <span className="meta-item meta-mode">
              <span className="meta-label">模式</span>
              <span className="meta-value">应用级 Loopback</span>
            </span>
          </div>

          {/* ISSUE-2：导出进度条（不确定模式，不造假百分比） */}
          {status === "exporting" && (
            <div className="export-progress-bar" aria-hidden />
          )}
        </div>

        {/* 错误提示：通用错误（errorMsg）或 ASR 错误（asrErrorMsg，error 态附「重启 ASR」按钮在控制条） */}
        {(errorMsg || asrErrorMsg) && (
          <div className="capture-notice error">{errorMsg || asrErrorMsg}</div>
        )}

        {/* 音频播放器 */}
        {audioSrc && (
          <div className="audio-player card">
            <div className="audio-label">录制回放：{audioLabel}</div>
            <audio
              ref={audioRef}
              controls
              className="audio-element"
              src={audioSrc}
            >
              您的浏览器不支持音频播放
            </audio>
          </div>
        )}
      </main>

      {/* ISSUE-4 词典设置模态 */}
      {showDict && <DictionaryModal onClose={() => setShowDict(false)} />}

      {/* ISSUE-3 导出设置模态（无条件可达） */}
      {showExport && <ExportSettingsModal onClose={() => setShowExport(false)} />}

      {/* F-14 设置模态 */}
      {showSettings && llmConfig && (
        <SettingsModal
          config={llmConfig}
          onChange={setLlmConfig}
          onSave={saveConfig}
          onClose={() => setShowSettings(false)}
        />
      )}
    </div>
  );
}

export default App;
