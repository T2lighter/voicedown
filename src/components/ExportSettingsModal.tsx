// ISSUE-3 导出设置模态（F 候选 F1 自 App.tsx 拆出）：enabled 开关 + txt/md 单选 + 路径纯文本输入。
// 镜像 DictionaryModal 范式（加载 + debounce 自动保存），多一层路径校验门控
// （实时调 validate_export_path：非法红字 + 暂停保存）。无条件可达（纯音频模式也用）。
import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";

// ISSUE-3：导出配置（与后端 ExportConfig 对齐；无条件可达，纯音频模式也用）
export interface ExportConfig {
  enabled: boolean;
  text_format: "txt" | "md";
  export_dir: string;
}

export function ExportSettingsModal({ onClose }: { onClose: () => void }) {
  const [config, setConfig] = useState<ExportConfig | null>(null);
  const [pathErr, setPathErr] = useState<string>("");
  const [saveStatus, setSaveStatus] = useState<"idle" | "saving" | "saved">(
    "idle"
  );
  const lastSavedRef = useRef("");

  // 打开时加载
  useEffect(() => {
    invoke<ExportConfig>("get_export_config").then((c) => {
      setConfig(c);
      lastSavedRef.current = JSON.stringify(c);
    });
  }, []);

  // 路径变化 debounce 校验（300ms）：红字 + 禁保存
  useEffect(() => {
    if (!config) return;
    const dir = config.export_dir;
    const t = setTimeout(() => {
      invoke("validate_export_path", { path: dir })
        .then(() => setPathErr(""))
        .catch((e) => setPathErr(String(e)));
    }, 300);
    return () => clearTimeout(t);
  }, [config?.export_dir]); // eslint-disable-line react-hooks/exhaustive-deps

  // config 变化 debounce 保存（路径合法 + 内容变化时才存）
  useEffect(() => {
    if (!config || pathErr) return;
    const sig = JSON.stringify(config);
    if (sig === lastSavedRef.current) return;
    setSaveStatus("saving");
    const t = setTimeout(() => {
      invoke("set_export_config", { config })
        .then(() => {
          lastSavedRef.current = sig;
          setSaveStatus("saved");
        })
        .catch(() => setSaveStatus("idle"));
    }, 600);
    return () => clearTimeout(t);
  }, [config, pathErr]);

  if (!config) return null;
  const patch = (p: Partial<ExportConfig>) => setConfig({ ...config, ...p });

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>导出设置</h2>
          <button className="close-btn" onClick={onClose} title="关闭">
            ×
          </button>
        </div>
        <div className="modal-body">
          <div className="row">
            <div className="row-label">
              <span className="row-title">启用导出</span>
              <span className="hint">停止捕获后自动保存音频与文本</span>
            </div>
            <label className="switch">
              <input
                type="checkbox"
                checked={config.enabled}
                onChange={(e) => patch({ enabled: e.target.checked })}
              />
              <span className="slider" />
            </label>
          </div>

          <div className="field">
            <span className="field-label">文本格式</span>
            <div className="segmented">
              <button
                className={config.text_format === "txt" ? "active" : ""}
                onClick={() => patch({ text_format: "txt" })}
              >
                TXT（纯文本）
              </button>
              <button
                className={config.text_format === "md" ? "active" : ""}
                onClick={() => patch({ text_format: "md" })}
              >
                Markdown
              </button>
            </div>
            <span className="hint">
              原文与优化文本共用此格式。Markdown = 内容 + 标题（flat，非结构化定稿）。
            </span>
          </div>

          <div className="field">
            <span className="field-label">导出目录</span>
            <input
              className="text-input"
              value={config.export_dir}
              onChange={(e) => patch({ export_dir: e.target.value })}
            />
            {pathErr ? (
              <span className="hint warn">{pathErr}</span>
            ) : (
              <span className="hint">不存在目录将自动创建。</span>
            )}
          </div>

          <div className="hint">
            音频格式固定为 WAV。关闭导出后停止捕获仅完成转录，不产文件。
          </div>

          <span className="save-indicator">
            {saveStatus === "saving"
              ? "保存中…"
              : saveStatus === "saved"
              ? "✓ 已自动保存"
              : pathErr
              ? "路径非法，暂停保存"
              : ""}
          </span>
        </div>
      </div>
    </div>
  );
}
