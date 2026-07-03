// 自定义窗口下拉组件（F 候选 F1 自 App.tsx 拆出）。
// 用原生 <select> 时，其弹出列表宽度由最长 option 决定（窗口标题可能极长），
// 且 CSS 无法约束该 popup 宽度，导致展开后横向无限扩展。改用自定义组件后，
// 弹出列表以触发器宽度为锚（position:absolute; left:0; right:0），严格等宽，
// 长标题用 ellipsis 截断、title 悬停看全文。
import { useState, useEffect, useRef } from "react";
import type { WindowInfo } from "../types";

export function WindowDropdown({
  windows,
  selectedPid,
  onSelect,
  disabled,
}: {
  windows: WindowInfo[];
  selectedPid: number | null;
  onSelect: (pid: number | null) => void;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // 展开时：点击外部 / ESC 关闭
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const selected = windows.find((w) => w.pid === selectedPid) ?? null;
  const selectedLabel = selected
    ? `[${selected.process_name}] ${selected.title}`
    : "-- 选择目标窗口 --";

  const choose = (pid: number | null) => {
    onSelect(pid);
    setOpen(false);
  };

  return (
    <div
      className={`window-dropdown ${open ? "open" : ""} ${
        disabled ? "disabled" : ""
      }`}
      ref={ref}
    >
      <button
        type="button"
        className="dropdown-trigger"
        onClick={() => !disabled && setOpen((o) => !o)}
        disabled={disabled}
      >
        <span className="dropdown-trigger-label" title={selectedLabel}>
          {selectedLabel}
        </span>
        <span className="dropdown-caret" aria-hidden>
          ▾
        </span>
      </button>
      {open && !disabled && (
        <ul className="dropdown-list" role="listbox">
          <li
            className={`dropdown-item ${selectedPid === null ? "active" : ""}`}
            onClick={() => choose(null)}
            title="-- 选择目标窗口 --"
          >
            -- 选择目标窗口 --
          </li>
          {windows.map((w) => {
            const text = `[${w.process_name}] ${w.title}`;
            return (
              <li
                key={w.hwnd}
                className={`dropdown-item ${
                  w.pid === selectedPid ? "active" : ""
                }`}
                onClick={() => choose(w.pid)}
                title={text}
              >
                {text}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
