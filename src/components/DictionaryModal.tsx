// ISSUE-4 词典编辑模态（F 候选 F1 自 App.tsx 拆出）：key→value 行增删改，debounce 自动保存。
// 后端 dictionary.json（BTreeMap）；英文 key 整词匹配、中文 key 子串匹配（text_postprocess::apply_dictionary）。
// ponytail: 行用 index 作 key——删除中间行可能丢 input 焦点，词典编辑场景可接受；要稳态焦点换 stable id。
import { useState, useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";

interface DictEntry {
  key: string;
  value: string;
}

export function DictionaryModal({ onClose }: { onClose: () => void }) {
  const [entries, setEntries] = useState<DictEntry[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [saveStatus, setSaveStatus] = useState<"idle" | "saving" | "saved">(
    "idle"
  );
  const lastSavedRef = useRef("");

  // entries → object（空 key 丢弃，重复 key 后者覆盖）。
  const toObj = useCallback((es: DictEntry[]): Record<string, string> => {
    const o: Record<string, string> = {};
    for (const e of es) {
      const k = e.key.trim();
      if (k) o[k] = e.value;
    }
    return o;
  }, []);

  // 打开时加载词典。
  useEffect(() => {
    invoke<Record<string, string>>("get_dictionary")
      .then((d) => {
        const es = Object.entries(d).map(([k, v]) => ({ key: k, value: v }));
        lastSavedRef.current = JSON.stringify(toObj(es));
        setEntries(es);
      })
      .catch(() => setEntries([]))
      .finally(() => setLoaded(true));
  }, [toObj]);

  // entries 变化 debounce 600ms 持久化（内容未变/加载回写不重复保存）。
  useEffect(() => {
    if (!loaded) return;
    const obj = toObj(entries);
    const sig = JSON.stringify(obj);
    if (sig === lastSavedRef.current) return;
    setSaveStatus("saving");
    const t = setTimeout(() => {
      invoke("set_dictionary", { dict: obj })
        .then(() => {
          lastSavedRef.current = sig;
          setSaveStatus("saved");
        })
        .catch(() => setSaveStatus("idle"));
    }, 600);
    return () => clearTimeout(t);
  }, [entries, loaded, toObj]);

  const patch = (i: number, p: Partial<DictEntry>) =>
    setEntries((es) => es.map((e, idx) => (idx === i ? { ...e, ...p } : e)));
  const remove = (i: number) =>
    setEntries((es) => es.filter((_, idx) => idx !== i));
  const add = () => setEntries((es) => [...es, { key: "", value: "" }]);

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>词典设置</h2>
          <button className="close-btn" onClick={onClose} title="关闭">
            ×
          </button>
        </div>
        <div className="modal-body">
          <div className="hint">
            ASR 文本按「原词 → 替换为」替换。英文 key 仅整词匹配（不误伤子串，
            如 <code>ai</code> 不改 <code>rain</code>）；中文 key 子串匹配。
          </div>

          {entries.map((e, i) => (
            <div className="dict-row" key={i}>
              <input
                className="text-input dict-key"
                value={e.key}
                onChange={(ev) => patch(i, { key: ev.target.value })}
                placeholder="原词"
              />
              <span className="dict-arrow">→</span>
              <input
                className="text-input dict-val"
                value={e.value}
                onChange={(ev) => patch(i, { value: ev.target.value })}
                placeholder="替换为"
              />
              <button
                className="icon-btn dict-del"
                onClick={() => remove(i)}
                title="删除"
              >
                ✕
              </button>
            </div>
          ))}

          <button className="btn btn-ghost" onClick={add}>
            + 添加
          </button>

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
  );
}
