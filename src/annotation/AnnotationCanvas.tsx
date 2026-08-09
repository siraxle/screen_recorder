import { useEffect, useRef, useState } from "react";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import {
  EVENT_CLEAR,
  EVENT_CLOSED,
  EVENT_SET_COLOR,
  EVENT_SET_TOOL,
  EVENT_UNDO,
  PRESET_COLORS,
  Shape,
  ShapeType,
  ToolMode,
} from "./types";

const STROKE_WIDTH = 4;

function renderShape(shape: Shape) {
  const x = Math.min(shape.x1, shape.x2);
  const y = Math.min(shape.y1, shape.y2);
  const width = Math.abs(shape.x2 - shape.x1);
  const height = Math.abs(shape.y2 - shape.y1);

  switch (shape.type) {
    case "rectangle":
      return (
        <rect
          key={shape.id}
          x={x}
          y={y}
          width={width}
          height={height}
          stroke={shape.color}
          strokeWidth={STROKE_WIDTH}
          fill="none"
        />
      );
    case "oval":
      return (
        <ellipse
          key={shape.id}
          cx={x + width / 2}
          cy={y + height / 2}
          rx={width / 2}
          ry={height / 2}
          stroke={shape.color}
          strokeWidth={STROKE_WIDTH}
          fill="none"
        />
      );
    case "line":
      return (
        <line
          key={shape.id}
          x1={shape.x1}
          y1={shape.y1}
          x2={shape.x2}
          y2={shape.y2}
          stroke={shape.color}
          strokeWidth={STROKE_WIDTH}
        />
      );
  }
}

export default function AnnotationCanvas() {
  const [tool, setTool] = useState<ToolMode>("pointer");
  const [color, setColor] = useState(PRESET_COLORS[0]);
  const [shapes, setShapes] = useState<Shape[]>([]);
  const [draft, setDraft] = useState<Shape | null>(null);
  const draftStart = useRef<{ x1: number; y1: number } | null>(null);

  // The overlay must be see-through, but the shared App.css sets a solid
  // background on :root for the main recorder window — override it here so
  // this window stays transparent.
  useEffect(() => {
    document.documentElement.style.background = "transparent";
    document.body.style.background = "transparent";
  }, []);

  useEffect(() => {
    getCurrentWindow()
      .setIgnoreCursorEvents(tool === "pointer")
      .catch(() => {});
  }, [tool]);

  useEffect(() => {
    const unlistenTool = listen<ToolMode>(EVENT_SET_TOOL, (e) => setTool(e.payload));
    const unlistenColor = listen<string>(EVENT_SET_COLOR, (e) => setColor(e.payload));
    const unlistenUndo = listen(EVENT_UNDO, () => setShapes((s) => s.slice(0, -1)));
    const unlistenClear = listen(EVENT_CLEAR, () => setShapes([]));
    return () => {
      unlistenTool.then((f) => f());
      unlistenColor.then((f) => f());
      unlistenUndo.then((f) => f());
      unlistenClear.then((f) => f());
    };
  }, []);

  // This window briefly holds OS focus while the user is dragging out a
  // shape (see handleMouseDown), so Esc has to be handled here too, not just
  // in the toolbar. Mid-drag, Esc cancels just the in-progress shape;
  // otherwise it closes the whole overlay, same as the toolbar's Close button.
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (draftStart.current) {
        draftStart.current = null;
        setDraft(null);
        return;
      }
      emit(EVENT_CLOSED);
      invoke("hide_annotation_overlay");
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  const handleMouseDown = (e: React.MouseEvent<SVGSVGElement>) => {
    if (tool === "pointer") return;
    draftStart.current = { x1: e.clientX, y1: e.clientY };
    setDraft({ id: "draft", type: tool, x1: e.clientX, y1: e.clientY, x2: e.clientX, y2: e.clientY, color });
  };

  const handleMouseMove = (e: React.MouseEvent<SVGSVGElement>) => {
    if (!draftStart.current) return;
    setDraft((d) => (d ? { ...d, x2: e.clientX, y2: e.clientY } : d));
  };

  const handleMouseUp = (e: React.MouseEvent<SVGSVGElement>) => {
    const start = draftStart.current;
    draftStart.current = null;
    if (!start) return;
    const { x1, y1 } = start;
    const x2 = e.clientX;
    const y2 = e.clientY;
    setDraft(null);
    if (Math.abs(x2 - x1) < 2 && Math.abs(y2 - y1) < 2) return;
    setShapes((s) => [
      ...s,
      { id: crypto.randomUUID(), type: tool as ShapeType, x1, y1, x2, y2, color },
    ]);
    // Deselect the drawing tool once a shape is placed, so the toolbar
    // reverts to its inactive state and the user can close it right away
    // instead of staying stuck in draw mode.
    setTool("pointer");
    emit(EVENT_SET_TOOL, "pointer");
  };

  return (
    <svg
      className="annotation-canvas"
      style={{ cursor: tool === "pointer" ? "default" : "crosshair" }}
      onMouseDown={handleMouseDown}
      onMouseMove={handleMouseMove}
      onMouseUp={handleMouseUp}
    >
      {shapes.map(renderShape)}
      {draft && renderShape(draft)}
    </svg>
  );
}
