import { useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  DEPENDENCY_LABELS,
  EVENT_DEPENDENCIES_CHANGED,
  EVENT_DEPENDENCY_INSTALL_PROGRESS,
  type DependencyId,
  type DependencyReport,
  type InstallProgress,
  type ScannedCandidate,
} from "../settings/types";
import "../App.css";

/** GPU packages are pip extras installed into the app's own managed Python
 * venv -- there's no standalone "already on your PC" copy of them to scan
 * for or browse to (see setup::apply_override on the Rust side), so this
 * row never gets the locate-existing-install controls the other two do. */
const LOCATABLE_IDS: DependencyId[] = ["python", "ffmpeg"];

function formatBytes(bytes: number): string {
  if (bytes <= 0) return "";
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `~${(mb / 1024).toFixed(1)} GB` : `~${Math.round(mb)} MB`;
}

const STAGE_LABELS: Record<InstallProgress["stage"], string> = {
  downloading: "Downloading",
  extracting: "Extracting",
  installing: "Installing",
  done: "Done",
};

interface DependencyPanelProps {
  /** Called with the freshest report on mount and whenever it changes, so a
   * parent (e.g. App.tsx's first-run gate) doesn't need a second `invoke`. */
  onReport?: (report: DependencyReport) => void;
}

/** Shared "check + install dependencies" UI, used both as a Settings tab
 * and as the first-run gate in the main window (see App.tsx). Self-
 * contained: fetches its own status and stays live via Tauri events. */
function DependencyPanel({ onReport }: DependencyPanelProps) {
  const [report, setReport] = useState<DependencyReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [installingId, setInstallingId] = useState<DependencyId | null>(null);
  const [confirmingId, setConfirmingId] = useState<DependencyId | null>(null);
  const [progressById, setProgressById] = useState<Partial<Record<DependencyId, InstallProgress>>>({});
  const [scanning, setScanning] = useState(false);
  const [scanResults, setScanResults] = useState<ScannedCandidate[] | null>(null);
  const [locatingId, setLocatingId] = useState<DependencyId | null>(null);
  const [locateErrorById, setLocateErrorById] = useState<Partial<Record<DependencyId, string>>>({});

  function applyReport(next: DependencyReport) {
    setReport(next);
    onReport?.(next);
  }

  useEffect(() => {
    invoke<DependencyReport>("check_dependencies").then(applyReport).catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const unlistenReport = listen<DependencyReport>(EVENT_DEPENDENCIES_CHANGED, (event) => {
      applyReport(event.payload);
    });
    const unlistenProgress = listen<InstallProgress>(EVENT_DEPENDENCY_INSTALL_PROGRESS, (event) => {
      setProgressById((prev) => ({ ...prev, [event.payload.dependency]: event.payload }));
    });
    return () => {
      unlistenReport.then((f) => f());
      unlistenProgress.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function handleInstall(id: DependencyId) {
    setConfirmingId(null);
    setInstallingId(id);
    setError(null);
    try {
      await invoke("install_dependencies", { targets: [id] });
    } catch (e) {
      setError(String(e));
    } finally {
      setInstallingId(null);
    }
  }

  async function handleScan() {
    setScanning(true);
    setError(null);
    try {
      const results = await invoke<ScannedCandidate[]>("scan_dependencies");
      setScanResults(results);
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  }

  async function handleUseCandidate(candidate: ScannedCandidate) {
    setLocatingId(candidate.id);
    setLocateErrorById((prev) => ({ ...prev, [candidate.id]: undefined }));
    try {
      const next = await invoke<DependencyReport>("use_dependency_path", {
        id: candidate.id,
        path: candidate.path,
        source: "system",
      });
      applyReport(next);
      setScanResults((prev) => prev?.filter((c) => c.id !== candidate.id) ?? null);
    } catch (e) {
      setLocateErrorById((prev) => ({ ...prev, [candidate.id]: String(e) }));
    } finally {
      setLocatingId(null);
    }
  }

  async function handleBrowse(id: DependencyId) {
    setLocateErrorById((prev) => ({ ...prev, [id]: undefined }));
    let path: string | null;
    try {
      path = await invoke<string | null>("pick_dependency_folder");
    } catch (e) {
      setLocateErrorById((prev) => ({ ...prev, [id]: String(e) }));
      return;
    }
    if (!path) return; // user cancelled the dialog

    setLocatingId(id);
    try {
      const next = await invoke<DependencyReport>("use_dependency_path", { id, path, source: "custom" });
      applyReport(next);
    } catch (e) {
      setLocateErrorById((prev) => ({ ...prev, [id]: String(e) }));
    } finally {
      setLocatingId(null);
    }
  }

  async function handleClearOverride(id: DependencyId) {
    setLocatingId(id);
    setLocateErrorById((prev) => ({ ...prev, [id]: undefined }));
    try {
      const next = await invoke<DependencyReport>("clear_dependency_override", { id });
      applyReport(next);
    } catch (e) {
      setLocateErrorById((prev) => ({ ...prev, [id]: String(e) }));
    } finally {
      setLocatingId(null);
    }
  }

  if (!report) {
    return error ? <p className="error">{error}</p> : <p className="output">Checking dependencies…</p>;
  }

  return (
    <div className="dependency-panel">
      {error && <p className="error">{error}</p>}

      <div className="panel dependency-row">
        <div className="controls">
          <button disabled={scanning} onClick={handleScan}>
            {scanning ? "Scanning…" : "Scan my PC for existing installs"}
          </button>
        </div>
        {scanResults && scanResults.length === 0 && (
          <p className="hint">
            No existing Python or FFmpeg install found nearby — try "Browse for folder…" on a row below.
          </p>
        )}
      </div>

      {report.statuses.map((status) => {
        const progress = progressById[status.id];
        const isInstalling = installingId === status.id;
        const pct = progress ? Math.round(progress.progress * 100) : 0;

        return (
          <div key={status.id} className="panel dependency-row">
            <div className="dependency-row-header">
              <strong>{DEPENDENCY_LABELS[status.id]}</strong>
              {!status.applicable ? (
                <span className="output"> {status.detail}</span>
              ) : status.satisfied ? (
                <span className="output"> ✓ {status.detail}</span>
              ) : (
                <span className="warning"> {status.detail}</span>
              )}
            </div>

            {status.applicable && isInstalling && (
              <div className="progress-bar">
                <div
                  className="progress-bar-fill"
                  role="progressbar"
                  aria-valuenow={pct}
                  aria-valuemin={0}
                  aria-valuemax={100}
                  style={{ "--progress": `${pct}%` } as CSSProperties}
                />
                <span className="progress-bar-label">
                  {progress ? `${STAGE_LABELS[progress.stage]} ${pct}%` : "Starting…"}
                </span>
              </div>
            )}

            {status.applicable && !isInstalling && confirmingId === status.id && (
              <div className="dependency-confirm">
                <p className="hint">
                  This will download and install {DEPENDENCY_LABELS[status.id]}
                  {status.approxDownloadBytes > 0 ? ` (${formatBytes(status.approxDownloadBytes)})` : ""}.
                </p>
                <div className="controls">
                  <button onClick={() => handleInstall(status.id)}>Confirm and install</button>
                  <button onClick={() => setConfirmingId(null)}>Cancel</button>
                </div>
              </div>
            )}

            {status.applicable && !isInstalling && confirmingId !== status.id && (
              <div className="controls">
                {!status.satisfied && (
                  <button onClick={() => setConfirmingId(status.id)}>
                    Install{status.approxDownloadBytes > 0 ? ` (${formatBytes(status.approxDownloadBytes)})` : ""}
                  </button>
                )}
                {status.satisfied && <button onClick={() => setConfirmingId(status.id)}>Reinstall</button>}
              </div>
            )}

            {status.applicable && LOCATABLE_IDS.includes(status.id) && (
              <div className="dependency-locate">
                {status.source !== "bundled" ? (
                  <div className="controls">
                    <span className="hint">
                      Using your {status.source === "system" ? "detected" : "custom"} install at {status.overridePath}
                    </span>
                    <button disabled={locatingId === status.id} onClick={() => handleClearOverride(status.id)}>
                      Use bundled install instead
                    </button>
                  </div>
                ) : (
                  <>
                    <div className="controls">
                      <button disabled={locatingId === status.id} onClick={() => handleBrowse(status.id)}>
                        Browse for folder…
                      </button>
                    </div>
                    {scanResults?.find((c) => c.id === status.id) && (
                      <div className="dependency-confirm">
                        <p className="hint">Found {scanResults.find((c) => c.id === status.id)!.detail}</p>
                        <div className="controls">
                          <button
                            disabled={locatingId === status.id}
                            onClick={() => handleUseCandidate(scanResults!.find((c) => c.id === status.id)!)}
                          >
                            Use this
                          </button>
                          <button
                            onClick={() => setScanResults((prev) => prev?.filter((c) => c.id !== status.id) ?? null)}
                          >
                            Dismiss
                          </button>
                        </div>
                      </div>
                    )}
                  </>
                )}
                {locateErrorById[status.id] && <p className="error">{locateErrorById[status.id]}</p>}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

export default DependencyPanel;
