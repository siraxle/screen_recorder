"""Batch transcription worker for NVIDIA Parakeet-TDT-0.6B-v3, invoked as a
subprocess by the Rust pipeline (see src-tauri/src/pipeline/stt.rs). Speaks
the exact same newline-delimited JSON stdout protocol as whisper_worker.py
(see that file's module docstring for the contract) -- this is the swappable
STT-runtime boundary CLAUDE.md calls for, so the Rust side needed no protocol
changes to support a second engine.

Runs the model via the `onnx-asr` package rather than NVIDIA's NeMo toolkit:
NeMo is CUDA-only and pulls in a heavy PyTorch stack, while `onnx-asr` runs
the same checkpoint through ONNX Runtime on either CPU or CUDA execution
providers, matching this app's per-hardware-swappable-runtime rule.

Unlike faster-whisper, the Parakeet-TDT-v3 checkpoint takes no language hint
at inference time and never reports a detected language back -- it is one
multilingual model with no per-utterance language ID output. So `--language`
here is purely a label echoed back into the `language` result field (typically
the user's manual override); when omitted, "und" (ISO 639-2 "undetermined")
is reported rather than guessing.

The model caps single-utterance input at roughly 20-30 seconds, so long
recordings are chunked with Silero VAD (`.with_vad(...)`) the same way
whisper_worker.py's `vad_filter=True` avoids hallucinated output on silence --
each VAD-detected speech segment becomes one transcript segment here.
"""

import argparse
import json
import os
import sys
import wave


def _register_cuda_dll_dirs() -> None:
    """onnxruntime-gpu's CUDA execution provider links against cuDNN 9, which
    this machine doesn't have as a system install. The `nvidia-cudnn-cu12`
    package (already installed for faster-whisper/CTranslate2, see
    whisper_worker.py's `_register_cuda_dll_dirs`) vendors the exact same
    `cudnn64_9.dll` onnxruntime needs, but onnxruntime resolves it the same
    plain-`LoadLibraryW` way CTranslate2 does -- `os.add_dll_directory` is
    silently ignored, so only prepending the package's `bin/` dir to `PATH`
    (classic DLL search order) makes it resolvable."""
    if os.name != "nt":
        return
    try:
        import nvidia.cublas
        import nvidia.cudnn
    except ImportError:
        return
    for package in (nvidia.cublas, nvidia.cudnn):
        bin_dir = os.path.join(package.__path__[0], "bin")
        if os.path.isdir(bin_dir):
            os.environ["PATH"] = bin_dir + os.pathsep + os.environ["PATH"]


def _wav_duration_seconds(path: str) -> float:
    """The extracted WAV is always 16kHz mono PCM (see
    capture::ffmpeg::build_wav_extract_command), so the stdlib `wave` module
    can read its duration directly -- no extra dependency needed just for the
    progress denominator."""
    with wave.open(path, "rb") as f:
        return f.getnframes() / float(f.getframerate())


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--audio", required=True, help="path to a 16kHz mono WAV file")
    parser.add_argument(
        "--language",
        default=None,
        help="ISO 639-1 code to echo back in the result; Parakeet has no auto-detect",
    )
    parser.add_argument("--device", default="cuda")
    args = parser.parse_args()

    _register_cuda_dll_dirs()
    import onnx_asr

    # Listing CUDAExecutionProvider first with CPUExecutionProvider as a
    # fallback lets onnxruntime degrade gracefully (with a stderr warning,
    # not a crash) on a machine without a CUDA-capable onnxruntime build
    # installed -- mirrors faster-whisper's --device flag one level up in
    # pipeline/stt.rs, without needing this worker to detect hardware itself.
    providers = (
        ["CUDAExecutionProvider", "CPUExecutionProvider"]
        if args.device == "cuda"
        else ["CPUExecutionProvider"]
    )

    vad = onnx_asr.load_vad("silero", providers=providers)
    # batch_size=1: onnx-asr's default (8) groups up to 8 VAD segments into a
    # single model.recognize_batch call and only yields once the whole batch
    # comes back. A short/medium recording rarely has more than 8 segments,
    # so with the default every progress line would land in one burst at the
    # very end -- the progress bar would sit at 0% the whole time and then
    # jump to done. Forcing one segment per inference call trades a bit of
    # GPU batching efficiency for genuine incremental progress, matching
    # whisper_worker.py's per-segment streaming.
    model = onnx_asr.load_model("nemo-parakeet-tdt-0.6b-v3", providers=providers).with_vad(
        vad, batch_size=1
    )

    duration = _wav_duration_seconds(args.audio)

    segments = []
    for res in model.recognize(args.audio):
        text = res.text.strip()
        if not text:
            continue
        segments.append({"start": res.start, "end": res.end, "text": text})
        if duration > 0:
            _emit(
                {
                    "type": "progress",
                    "processed_seconds": min(res.end, duration),
                    "duration_seconds": duration,
                }
            )

    result = {
        "type": "result",
        "language": args.language or "und",
        "segments": segments,
    }
    _emit(result)


def _emit(obj: dict) -> None:
    """Writes one JSON object as a line and flushes immediately, so the Rust
    side can react to progress lines as they arrive instead of only after
    the whole subprocess exits."""
    json.dump(obj, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        import traceback

        traceback.print_exc(file=sys.stderr)
        sys.exit(1)
