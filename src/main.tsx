import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import AnnotationCanvas from "./annotation/AnnotationCanvas";
import AnnotationToolbar from "./annotation/AnnotationToolbar";
import SettingsWindow from "./settings/SettingsWindow";
import LiveSubtitlesWindow from "./live_subtitles/LiveSubtitlesWindow";
import { ErrorBoundary } from "./ErrorBoundary";

function RootForWindow() {
  switch (getCurrentWindow().label) {
    case "annotation-canvas":
      return <AnnotationCanvas />;
    case "annotation-toolbar":
      return <AnnotationToolbar />;
    case "settings":
      return <SettingsWindow />;
    case "live-subtitles":
      return <LiveSubtitlesWindow />;
    default:
      return <App />;
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>
      <RootForWindow />
    </ErrorBoundary>
  </React.StrictMode>,
);
