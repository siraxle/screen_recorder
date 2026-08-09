import { useEffect, useState } from "react";
import { emit, listen } from "@tauri-apps/api/event";
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

  // The canvas window resets itself to "pointer" as soon as a shape is
  // finished drawing, so the toolbar's own tool highlight needs to follow
  // that broadcast rather than only reacting to local clicks.
  useEffect(() => {
    const unlisten = listen<ToolMode>(EVENT_SET_TOOL, (e) => setTool(e.payload));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const selectTool = (mode: ToolMode) => {
    setTool(mode);
    emit(EVENT_SET_TOOL, mode);
  };

  const selectColor = (value: string) => {
    setColor(value);
    emit(EVENT_SET_COLOR, value);
  };

  const closeOverlay = () => {
    emit(EVENT_CLOSED);
    invoke("hide_annotation_overlay");
  };

  // Esc closes the whole overlay, same as the Close button — this window
  // holds OS focus whenever the user isn't actively dragging out a shape on
  // the canvas (see AnnotationCanvas.tsx for the drag-in-progress case).
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeOverlay();
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

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

      <div className="annotation-colors">
        {PRESET_COLORS.map((preset) => (
          <button
            key={preset}
            className={`annotation-swatch${color === preset ? " active" : ""}`}
            style={{ backgroundColor: preset }}
            onClick={() => selectColor(preset)}
            aria-label={`Color ${preset}`}
          />
        ))}
      </div>

      <span className="annotation-divider" />

      <button className="annotation-tool-btn" onClick={() => emit(EVENT_UNDO)}>
        Undo
      </button>
      <button className="annotation-tool-btn" onClick={() => emit(EVENT_CLEAR)}>
        Clear
      </button>
      <button className="annotation-tool-btn" onClick={closeOverlay}>
        Close
      </button>
    </div>
  );
}
