//! Windows system-audio (loopback) capture via WASAPI.
//!
//! ffmpeg's Windows demuxers (dshow/gdigrab) have no native WASAPI loopback
//! support without a third-party virtual audio driver (Stereo Mix /
//! VB-Cable), which we can't assume is installed. So the Rust core captures
//! loopback PCM directly and hands ffmpeg a raw-PCM file to mux in
//! afterwards (see `ffmpeg::build_mux_command`) — this is the "WASAPI
//! loopback" path called out separately from FFmpeg in CLAUDE.md's capture
//! table.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

pub const SAMPLE_RATE: u32 = 48000;
pub const CHANNELS: u16 = 2;

/// A running loopback capture writing raw interleaved f32le PCM to a file.
/// Drop without calling `stop` will leak the capture thread (it only exits
/// on the stop flag) — callers must always call `stop`.
#[cfg(target_os = "windows")]
pub struct LoopbackCapture {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

/// System audio loopback capture isn't implemented for this OS yet (Week 1
/// milestone scope is Windows only, see roadmap in screen-recorder-spec.md).
/// This stub keeps `controller.rs` platform-agnostic; it errors instead of
/// silently recording nothing if ever invoked.
#[cfg(not(target_os = "windows"))]
pub struct LoopbackCapture;

#[cfg(not(target_os = "windows"))]
impl LoopbackCapture {
    pub fn start(
        _output_path: &Path,
        _live_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<Self> {
        Err(anyhow::anyhow!(
            "system audio loopback capture is not implemented on this OS yet"
        ))
    }

    pub fn stop(self) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl LoopbackCapture {
    /// `live_tap`, when present, receives a best-effort clone of every chunk
    /// of PCM as it's captured (see F6 live subtitles) in addition to the
    /// normal file write -- a full or closed channel never affects the file
    /// write, since the on-disk recording must not depend on anything
    /// downstream consuming the tap.
    pub fn start(
        output_path: &Path,
        live_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let output_path = output_path.to_path_buf();

        let handle = std::thread::Builder::new()
            .name("system-audio-loopback".to_string())
            .spawn(move || windows_impl::capture_loop(&output_path, thread_stop_flag, live_tap))
            .map_err(|e| anyhow::anyhow!("failed to spawn loopback capture thread: {e}"))?;

        Ok(Self {
            stop_flag,
            handle: Some(handle),
        })
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("loopback capture thread panicked"))??;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    use super::{CHANNELS, SAMPLE_RATE};

    /// wasapi's error type isn't known to implement `std::error::Error`, so
    /// every fallible call is wrapped explicitly via Debug rather than
    /// relying on `anyhow`'s blanket `?` conversion.
    fn wasapi_err<E: std::fmt::Debug>(context: &'static str) -> impl FnOnce(E) -> anyhow::Error {
        move |e| anyhow!("{context}: {e:?}")
    }

    pub(super) fn capture_loop(
        output_path: &PathBuf,
        stop_flag: Arc<AtomicBool>,
        live_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<()> {
        initialize_mta()
            .ok()
            .map_err(wasapi_err("COM initialization failed"))?;

        let enumerator = DeviceEnumerator::new().map_err(wasapi_err("device enumerator"))?;
        // `Direction::Render` opens the default *playback* device for capture,
        // i.e. loopback: it yields whatever is being played on the system, not
        // whatever is being recorded through a microphone.
        let device = enumerator
            .get_default_device(&Direction::Render)
            .map_err(wasapi_err("no default playback device"))?;
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(wasapi_err("failed to open audio client"))?;

        let desired_format = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS as usize,
            None,
        );
        let (_default_period, min_period) = audio_client
            .get_device_period()
            .map_err(wasapi_err("failed to query device period"))?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_period,
        };
        // The device itself was opened via `Direction::Render` (the default
        // playback device); passing `Direction::Capture` here is what makes
        // wasapi set AUDCLNT_STREAMFLAGS_LOOPBACK — asking to *capture* from
        // a *render* device is the actual definition of loopback capture.
        audio_client
            .initialize_client(&desired_format, &Direction::Capture, &mode)
            .map_err(wasapi_err("failed to initialize loopback client"))?;

        let event_handle = audio_client
            .set_get_eventhandle()
            .map_err(wasapi_err("failed to get event handle"))?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(wasapi_err("failed to get capture client"))?;
        let mut sample_queue: VecDeque<u8> = VecDeque::with_capacity(4 * SAMPLE_RATE as usize);

        let mut out_file = File::create(output_path)?;

        audio_client
            .start_stream()
            .map_err(wasapi_err("failed to start loopback stream"))?;

        while !stop_flag.load(Ordering::SeqCst) {
            capture_client
                .read_from_device_to_deque(&mut sample_queue)
                .map_err(wasapi_err("loopback read failed"))?;
            if !sample_queue.is_empty() {
                let bytes: Vec<u8> = sample_queue.drain(..).collect();
                out_file.write_all(&bytes)?;
                if let Some(tap) = &live_tap {
                    let _ = tap.send(bytes);
                }
            }
            // Timeout is expected whenever the device is silent for a while;
            // only the stop flag decides whether the loop actually ends.
            let _ = event_handle.wait_for_event(200);
        }

        // Drain whatever arrived between the last event and the stop flag flip.
        capture_client
            .read_from_device_to_deque(&mut sample_queue)
            .map_err(wasapi_err("final loopback read failed"))?;
        if !sample_queue.is_empty() {
            let bytes: Vec<u8> = sample_queue.drain(..).collect();
            out_file.write_all(&bytes)?;
            if let Some(tap) = &live_tap {
                let _ = tap.send(bytes);
            }
        }

        audio_client
            .stop_stream()
            .map_err(wasapi_err("failed to stop loopback stream"))?;
        Ok(())
    }
}
