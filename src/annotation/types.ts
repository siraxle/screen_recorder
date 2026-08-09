export type ToolMode = "pointer" | "rectangle" | "oval" | "line";
export type ShapeType = Exclude<ToolMode, "pointer">;

export interface Shape {
  id: string;
  type: ShapeType;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  color: string;
}

export const PRESET_COLORS = [
  "#ff3b30", // red
  "#ff9500", // orange
  "#ffcc00", // yellow
  "#34c759", // green
  "#007aff", // blue
  "#5856d6", // purple
  "#000000", // black
  "#ffffff", // white
];

export const EVENT_SET_TOOL = "annotation:set-tool";
export const EVENT_SET_COLOR = "annotation:set-color";
export const EVENT_UNDO = "annotation:undo";
// Wipes drawn shapes on the canvas window. Emitted by the toolbar's Clear
// button, and by the Rust side right before hiding the overlay so it comes
// back blank next time (see src-tauri/src/overlay.rs::hide).
export const EVENT_CLEAR = "annotation:clear";
// Tells the main recorder window the overlay was closed — emitted by the
// toolbar's own Close button and Esc handling on the frontend, and by the
// Rust side (see src-tauri/src/overlay.rs::hide) so hotkey-triggered closes
// keep the main window's toggle button in sync too.
export const EVENT_CLOSED = "annotation:closed";
// Tells the main recorder window the overlay was opened from the global
// hotkey (see src-tauri/src/overlay.rs::show), so its toggle button can stay
// in sync even when the overlay wasn't opened from that button.
export const EVENT_OPENED = "annotation:opened";
