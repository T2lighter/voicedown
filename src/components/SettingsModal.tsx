// F-14 文本优化设置模态（F 候选 F1 自 App.tsx 拆出）。
// 后端选择（Ollama/OpenAI）+ endpoint/model/api_key 输入 + 启用开关 + 测试连接 +
// debounce 600ms 自动保存。config 由父组件持有（受控），onChange 上抛。
import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { LlmConfig } from "../types";

export function SettingsModal({
  config,
  onChange,
  onSave,
  onClose,
}: {
  config: LlmConfig;
  onChange: (c: LlmConfig) => void;
  onSave: (c: LlmConfig) => void;
  onClose: () => void;
}) {
  const [saveStatus, setSaveStatus] = useState<"idle" | "saving" | "saved">(
    "idle"
  );
  const [testStatus, setTestStatus] = useState<
    "idle" | "testing" | "success" | "fail"
  >("idle");
  const firstRef = useRef(true);

  // 自动保存：config 变化（含 API key）debounce 600ms 持久化；首次加载不触发。
  useEffect(() => {
    if (firstRef.current) {
      firstRef.current = false;
      return;
    }
    setSaveStatus("saving");
    const t = setTimeout(() => {
      onSave(config);
      setSaveStatus("saved");
    }, 600);
    return () => clearTimeout(t);
  }, [config, onSave]);

  // 任意配置改动（endpoint / key / model / 后端）都使上次测试结果失效，需重测。
  useEffect(() => {
    setTestStatus("idle");
  }, [config]);

  const handleTest = async () => {
    setTestStatus("testing");
    try {
      const ok: boolean = await invoke<boolean>("check_llm_available");
      setTestStatus(ok ? "success" : "fail");
    } catch (e) {
      console.error("[VoiceDown] 测试连接失败:", e);
      setTestStatus("fail");
    }
  };

  const patch = (p: Partial<LlmConfig>) => onChange({ ...config, ...p });

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>文本优化设置</h2>
          <button className="close-btn" onClick={onClose} title="关闭">
            ×
          </button>
        </div>
        <div className="modal-body">
          <div className="row">
            <div className="row-label">
              <span className="row-title">启用文本优化</span>
              <span className="hint">ASR 文本经 LLM 纠错、标点、润色</span>
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
            <span className="field-label">后端</span>
            <div className="segmented">
              <button
                className={config.backend === "ollama" ? "active" : ""}
                onClick={() => patch({ backend: "ollama" })}
              >
                Ollama 本地
              </button>
              <button
                className={config.backend === "openai" ? "active" : ""}
                onClick={() => patch({ backend: "openai" })}
              >
                云端 API
              </button>
            </div>
          </div>

          {config.backend === "ollama" ? (
            <>
              <div className="field">
                <span className="field-label">Ollama endpoint</span>
                <input
                  className="text-input"
                  value={config.ollama.endpoint}
                  onChange={(e) =>
                    patch({ ollama: { ...config.ollama, endpoint: e.target.value } })
                  }
                  placeholder="http://localhost:11434"
                />
              </div>
              <div className="field">
                <span className="field-label">模型</span>
                <input
                  className="text-input"
                  value={config.ollama.model}
                  onChange={(e) =>
                    patch({ ollama: { ...config.ollama, model: e.target.value } })
                  }
                  placeholder="qwen2.5:3b"
                />
              </div>
              <div className="hint">需先运行 Ollama 并拉取模型，如：ollama pull qwen2.5:3b</div>
            </>
          ) : (
            <>
              <div className="field">
                <span className="field-label">API endpoint</span>
                <input
                  className="text-input"
                  value={config.openai.endpoint}
                  onChange={(e) =>
                    patch({ openai: { ...config.openai, endpoint: e.target.value } })
                  }
                  placeholder="https://api.deepseek.com/v1"
                />
              </div>
              <div className="field">
                <span className="field-label">模型</span>
                <input
                  className="text-input"
                  value={config.openai.model}
                  onChange={(e) =>
                    patch({ openai: { ...config.openai, model: e.target.value } })
                  }
                  placeholder="deepseek-chat"
                />
              </div>
              <div className="field">
                <span className="field-label">API Key</span>
                <input
                  className="text-input"
                  type="password"
                  value={config.openai.api_key}
                  onChange={(e) =>
                    patch({ openai: { ...config.openai, api_key: e.target.value } })
                  }
                  placeholder="sk-..."
                />
              </div>
              <div className="hint warn">
                注意：云端后端会把 ASR 文本外发到该服务。填写后将自动保存到本地配置文件。
              </div>
            </>
          )}

          <div className="test-row">
            <button
              className="btn btn-ghost"
              onClick={handleTest}
              disabled={testStatus === "testing"}
            >
              {testStatus === "testing" ? "测试中…" : "测试连接"}
            </button>
            {testStatus === "testing" && (
              <span className="test-result testing">测试中…</span>
            )}
            {testStatus === "success" && (
              <span className="test-result success">✓ 连接成功</span>
            )}
            {testStatus === "fail" && (
              <span className="test-result fail">
                ✗ 连接失败，请检查 endpoint / API Key
              </span>
            )}
            <span className="save-indicator">
              {saveStatus === "saving"
                ? "保存中…"
                : saveStatus === "saved"
                ? "✓ 已自动保存"
                : ""}
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
