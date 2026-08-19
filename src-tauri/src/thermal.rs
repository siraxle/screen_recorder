//! GPU thermal guard for local LLM inference (translate/summarize). A local
//! LLM call keeps the GPU at sustained high utilization; unlike STT (one
//! subprocess call per recording), F8/F9 dual-subtitle translation makes one
//! LLM call *per transcript segment* back to back with no pauses, which has
//! driven this dev machine's GPU to a thermal shutdown. This module is the
//! fix: poll `nvidia-smi` and block the caller while the GPU is at or above
//! `PAUSE_AT_C`, resuming only once it drops back to `RESUME_AT_C` (a lower
//! threshold, not `PAUSE_AT_C` itself, so a temp oscillating right at the
//! pause line doesn't thrash start/stop every poll).
//!
//! NVIDIA-only (`nvidia-smi`): this app's other hardware-specific paths
//! (whisper.cpp on Apple Silicon/CPU, WhisperX, etc., per CLAUDE.md) don't
//! have an equivalent single cross-vendor temperature query, so on any
//! machine without `nvidia-smi` on PATH this guard is a no-op rather than a
//! blocker -- best-effort, matching CLAUDE.md's treatment of other
//! hardware-gapped features (e.g. Wayland capture).

use std::time::Duration;

/// GPU temp at/above which local LLM calls pause. Set by hand (not
/// discovered from hardware limits) per user direction: this machine
/// shut itself down during F8/F9 dual-subtitle translation, so 83 C is a
/// deliberately conservative ceiling rather than the card's actual
/// throttle/shutdown point. Lowered from 87 C after the ceiling still ran
/// too hot in practice.
pub const PAUSE_AT_C: u32 = 83;

/// Resume threshold, below `PAUSE_AT_C` so the guard doesn't toggle on
/// every poll while the GPU hovers near the ceiling.
pub const RESUME_AT_C: u32 = 76;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Parses `nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader,nounits`
/// output (a bare integer, e.g. `"72\n"`) into Celsius. Split out from the
/// subprocess call so the parsing logic is unit-testable without an actual
/// NVIDIA GPU present.
fn parse_temp_output(stdout: &str) -> Option<u32> {
    stdout.lines().next()?.trim().parse().ok()
}

/// Reads the current GPU temperature via `nvidia-smi`. Returns `Ok(None)`
/// (not an error) when `nvidia-smi` isn't on PATH or fails to run, so
/// non-NVIDIA machines never get blocked by this best-effort check.
/// Cheap "is there an NVIDIA GPU at all" check, reused by `setup::check_all`
/// (deciding whether to offer the GPU pip extras) and by STT's `Auto` device
/// resolution (deciding whether to pick `cuda`). Wraps the same `nvidia-smi`
/// presence probe `read_gpu_temp_celsius` already does -- a `None` reading
/// already means "no nvidia-smi / no GPU," this just names that as a bool so
/// callers that don't care about the actual temperature don't have to spawn
/// a second `nvidia-smi` process.
pub async fn gpu_present() -> bool {
    read_gpu_temp_celsius().await.is_some()
}

async fn read_gpu_temp_celsius() -> Option<u32> {
    let output = crate::procutil::command("nvidia-smi")
        .args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_temp_output(&String::from_utf8_lossy(&output.stdout))
}

/// Blocks until the GPU is below `RESUME_AT_C`, polling every
/// `POLL_INTERVAL`. Returns immediately (no polling overhead) if the GPU is
/// already under `PAUSE_AT_C`, or if temperature can't be read at all.
/// Called before each local-provider LLM call in the translate/summarize
/// pipeline stages -- see `pipeline::run_translation` and
/// `pipeline::run_summarization`.
///
/// `on_cooldown` fires once per poll while blocked (with the temp that
/// triggered it) so the caller can surface a "GPU cooling down" signal to
/// the UI -- mirrors `pipeline::run_transcription`'s `on_progress` callback
/// pattern rather than taking an `AppHandle` directly, so this stays
/// testable without a live Tauri runtime.
pub async fn wait_for_safe_temp(on_cooldown: &(dyn Fn(u32) + Send + Sync)) {
    let Some(mut temp) = read_gpu_temp_celsius().await else {
        return;
    };
    if temp < PAUSE_AT_C {
        return;
    }
    tracing::warn!("GPU at {temp}C, pausing local LLM inference until it drops to {RESUME_AT_C}C");
    while temp >= RESUME_AT_C {
        on_cooldown(temp);
        tokio::time::sleep(POLL_INTERVAL).await;
        match read_gpu_temp_celsius().await {
            Some(t) => temp = t,
            // Lost the ability to read temp mid-cooldown -- fail open rather
            // than blocking a job forever on a transient nvidia-smi hiccup.
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_temp_output_reads_bare_integer() {
        assert_eq!(parse_temp_output("72\n"), Some(72));
    }

    #[test]
    fn parse_temp_output_handles_no_trailing_newline() {
        assert_eq!(parse_temp_output("87"), Some(87));
    }

    #[test]
    fn parse_temp_output_rejects_garbage() {
        assert_eq!(parse_temp_output("N/A\n"), None);
        assert_eq!(parse_temp_output(""), None);
    }
}
