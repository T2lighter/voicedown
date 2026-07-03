// VoiceDown 前端共享类型（F 候选 F1：自 App.tsx 顶部抽出）。
//
// 事件/状态 type 跨 hook + 组件共享；纯私有 type（DictEntry 仅 DictionaryModal 用、
// ExportConfig 仅 ExportSettingsModal 用）就近放各自组件文件，不外泄。
//
// ⚠ 这些 type 与后端 Rust serde 字段靠肉眼对齐（H 候选 Speculative：三端协议单一事实源，
// 未做）。改字段时需同步 src-tauri 对应 struct。

export type CaptureStatus = "idle" | "capturing" | "stopping" | "exporting";

// ASR 状态机阶段（与后端 get_asr_state 返回值对齐，ISSUE-2 崩溃自愈）：
// loading 初始加载 / ready 就绪 / respawning 崩溃后自动重生中 / error 重生超限待手动重启 / unavailable 非 asr 编译
export type AsrPhase = "loading" | "ready" | "respawning" | "error" | "unavailable";

export interface WindowInfo {
  hwnd: number;
  title: string;
  process_name: string;
  pid: number;
}

export interface CaptureStatusInfo {
  samples: number;
  duration_secs: number;
  asr_active: boolean;
}

export interface TranscriptionEvent {
  text: string;
  full_text: string;
  language: string;
  is_final: boolean;
}

export interface LlmConfig {
  enabled: boolean;
  backend: "ollama" | "openai";
  ollama: { endpoint: string; model: string };
  openai: { endpoint: string; model: string; api_key: string };
}

export interface OptimizeEvent {
  optimized: string;
  full_optimized: string;
  is_final: boolean;
}

// 离线定稿事件（后端 finalize_document 线程 emit；final_path 存盘失败时 null）
export interface FinalizeEvent {
  final_text: string;
  final_path: string | null;
}

// ISSUE-2：导出进度 / 完成事件（后端 finalize 线程 emit；payload 形状与 Rust ExportProgress/ExportDone 对齐）
export interface ExportProgress {
  phase: string; // "draining" | "writing-audio" | "writing-text"
}

export interface ExportDone {
  wav_path: string | null;
  txt_path: string | null;
  optimized_path: string | null;
  duration_secs: number;
  skipped: string | null;
  error: string | null;
}
