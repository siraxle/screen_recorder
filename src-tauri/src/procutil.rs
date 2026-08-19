//! Subprocess spawning that never flashes a console window on Windows.
//!
//! The app itself is built with `windows_subsystem = "windows"` (see
//! `main.rs`), so it has no console of its own. But every child process this
//! app spawns (ffmpeg, ffprobe, python, HandBrakeCLI, nvidia-smi, `where`,
//! ...) is a console-subsystem executable, and Windows allocates a brand new
//! visible console window for each one unless the creating process opts out
//! via `CREATE_NO_WINDOW`. All subprocess spawning in this codebase must go
//! through `command`/`std_command` instead of `Command::new` directly, or a
//! console window flashes on screen for every ffmpeg/python invocation --
//! several times per second during live subtitles.

use std::ffi::OsStr;

/// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Builds a `tokio::process::Command` that won't pop up a console window on
/// Windows. Use this instead of `tokio::process::Command::new` everywhere.
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Same as [`command`], for call sites that need a blocking `std::process::Command`.
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
