"""Batch transcription worker, invoked as a subprocess by the Rust pipeline
(see src-tauri/src/pipeline/stt.rs).

Contract: takes a path to a normalized WAV file and prints newline-delimited
JSON objects to stdout: zero or more `{"type": "progress", ...}` lines as
segments are produced, followed by exactly one `{"type": "result", ...}`
line on success, then exits 0. On failure, prints a traceback to stderr and
exits non-zero (no result line is written). This is the swappable boundary
CLAUDE.md calls for between the Rust orchestrator and the STT runtime
(faster-whisper here, whisper.cpp elsewhere) -- it never touches ffmpeg or
the mp4 container itself.
"""

import argparse
import json
import os
import sys


def _register_cuda_dll_dirs() -> None:
    """CTranslate2's wheel links against CUDA 12's cuBLAS/cuDNN, which this
    machine doesn't have as a system install (it has the CUDA 13 toolkit).
    `pip install nvidia-cublas-cu12 nvidia-cudnn-cu12` provides those exact
    DLLs. CTranslate2 resolves them via a plain `LoadLibraryW("cublas64_12.dll")`
    (no `LOAD_LIBRARY_SEARCH_DEFAULT_DIRS` flag), so `os.add_dll_directory`
    is silently ignored here -- only prepending their `bin/` dirs to the
    `PATH` env var (part of the classic DLL search order) actually works.
    """
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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--audio", required=True, help="path to a 16kHz mono WAV file")
    parser.add_argument("--language", default=None, help="ISO 639-1 code, omit to auto-detect")
    parser.add_argument("--model", default="small")
    parser.add_argument("--device", default="cuda")
    parser.add_argument("--compute-type", default="float16")
    args = parser.parse_args()

    _register_cuda_dll_dirs()
    from faster_whisper import WhisperModel

    model = WhisperModel(args.model, device=args.device, compute_type=args.compute_type)

    # vad_filter=True runs Silero VAD before inference, per CLAUDE.md ("Always
    # run VAD before Whisper inference -- long silences cause hallucinated
    # output otherwise").
    segments_iter, info = model.transcribe(
        args.audio,
        language=args.language,
        vad_filter=True,
    )

    # `info.duration` (from the initial VAD/decode pass) is known before the
    # segment generator is consumed, so it's a stable denominator for
    # progress -- segments are then streamed and reported as they land,
    # since faster-whisper only yields them once each is fully decoded.
    duration = info.duration
    segments = []
    for segment in segments_iter:
        segments.append(
            {"start": segment.start, "end": segment.end, "text": segment.text.strip()}
        )
        if duration > 0:
            _emit(
                {
                    "type": "progress",
                    "processed_seconds": min(segment.end, duration),
                    "duration_seconds": duration,
                }
            )

    result = {
        "type": "result",
        "language": info.language,
        "language_probability": info.language_probability,
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
