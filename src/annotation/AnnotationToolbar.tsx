import { useState } from "react";
import { emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { EVENT_CLEAR, EVENT_CLOSED, EVENT_SET_COLOR, EVENT_SET_TOOL, EVENT_UNDO, PRESET_COLORS, ToolMode } from "./types";

const TOOLS: { mode: ToolMode; label: string }[] = [
  { mode: "pointer", label: "Pointer" },
  { mode: "rectangle", label: "Rect" },
  { mode: "oval", label: "Oval" },
  { mode: "line", label: "Line" },
];

export default function AnnotationToolbar() {
  const [tool, setTool] = useState<ToolMode>("pointer");
  const [color, setColor] = useState(PRESET_COLORS[0]);

  const selectTool = (mode: ToolMode) => {
    setTool(mode);
    emit(EVENT_SET_TOOL, mode);
  };

  const selectColor = (value: string) => {
    setColor(value);
    emit(EVENT_SET_COLOR, value);
  };

  return (
    <div className="annotation-toolbar">
      {TOOLS.map(({ mode, label }) => (
        <button
          key={mode}
          className={`annotation-tool-btn${tool === mode ? " active" : ""}`}
          onClick={() => selectTool(mode)}
        >
          {label}
        </button>
      ))}

      <span className="annotation-divider" />

      {PRESET_COLORS.map((preset) => (
        <button
          key={preset}
          className={`annotation-swatch${color === preset ? " active" : ""}`}
          style={{ backgroundColor: preset }}
          onClick={() => selectColor(preset)}
          aria-label={`Color ${preset}`}
        />
      ))}

      <span className="annotation-divider" />

      <button className="annotation-tool-btn" onClick={() => emit(EVENT_UNDO)}>
        Undo
      </button>
      <button className="annotation-tool-btn" onClick={() => emit(EVENT_CLEAR)}>
        Clear
      </button>
      <button
        className="annotation-tool-btn"
        onClick={() => {
          emit(EVENT_CLOSED);
          invoke("hide_annotation_overlay");
        }}
      >
        Close
      </button>
    </div>
  );
}
