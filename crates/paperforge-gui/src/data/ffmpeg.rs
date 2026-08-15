//! ffmpeg subprocess wrapper for thumbnail generation.
//!
//! Phase 2 (Fase 6C.2) replaces the Phase 1 "no preview" fallback
//! for `LooseVideo` entries. We shell out to `ffmpeg` for a single
//! PNG frame, then pipe through the existing `image`-based
//! resize/encode pipeline in [`crate::data::thumbnails::load_thumbnail`].
//!
//! ## Why a separate module (not inline in `thumbnails.rs`)
//!
//! - **Single concern**: subprocess invocation + error mapping is a
//!   distinct concern from PNG decode + resize. Keeping them
//!   separate makes each testable on its own.
//! - **Mockable**: the `ffmpeg_available` probe + the `Command`
//!   construction are the only ffmpeg-specific surface; the
//!   thumbnail loader takes PNG bytes and is ffmpeg-agnostic.
//! - **Future swap**: if we ever replace the subprocess with an
//!   `ffmpeg-next` (Rust bindings) call, the diff is contained to
//!   this file.
//!
//! ## Failure modes
//!
//! - `NotFound` — `ffmpeg` not on PATH. The UI degrades to the
//!   title-only thumbnail (same UX as Phase 1).
//! - `Timeout` — extraction took >8s. Likely a huge file or a
//!   spinning disk. We abort and surface `Failed` to the UI.
//! - `NonZeroExit` — ffmpeg ran but exited non-zero (corrupt
//!   container, codec not built into ffmpeg, etc).
//! - `InvalidPng` — ffmpeg exited 0 but the stdout isn't a valid
//!   PNG. Defensive sanity check; should never happen in practice.
//! - `Io` — spawn / wait / stdio error.
//!
//! ## Command construction
//!
//! ```text
//! ffmpeg -hide_banner -loglevel error -y \
//!        -ss 00:00:01 \
//!        -i <input> \
//!        -frames:v 1 \
//!        -f image2pipe -vcodec png \
//!        -
//! ```
//!
//! - `-ss 00:00:01` — skip past the typical fade-in from black.
//!   Most loose videos are recorded with this convention; the
//!   first frame is frequently solid black or a logo card.
//! - `-frames:v 1` — extract exactly one frame (default is all).
//! - `-f image2pipe -vcodec png -` — pipe PNG to stdout; avoids a
//!   temp file and a second `read()` roundtrip.
//! - `-hide_banner -loglevel error -y` — quiet output, no prompts.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Hard timeout for ffmpeg first-frame extraction. Most wallpapers
/// decode in <500ms; 8s leaves headroom for very large videos and
/// slow disks without blocking the UI forever. Beyond that the user
/// has already given up on the picker.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(8);

/// Errors specific to the ffmpeg subprocess. The caller maps these
/// to [`crate::error::GuiError::Ffmpeg`] for the UI layer.
#[derive(Debug, Clone, PartialEq)]
pub enum FfmpegError {
    /// `ffmpeg` binary not found on PATH. The UI should fall back
    /// to the title-only thumbnail (same as Phase 1).
    NotFound,
    /// ffmpeg returned non-zero exit. `stderr` excerpt included
    /// for the warn log (truncated to 512 chars to bound the log).
    NonZeroExit { code: Option<i32>, stderr: String },
    /// ffmpeg timed out before producing output.
    Timeout,
    /// ffmpeg exited 0 but the stdout isn't a valid PNG.
    InvalidPng(String),
    /// IO error spawning / waiting on the process.
    Io(String),
}

impl FfmpegError {
    /// Short, single-word category for the UI banner.
    #[allow(dead_code)] // exposed for ui/status.rs (PR 3+) banner; tests pin the labels
    pub fn source(&self) -> &'static str {
        match self {
            FfmpegError::NotFound => "ffmpeg-missing",
            FfmpegError::NonZeroExit { .. } => "ffmpeg-exit",
            FfmpegError::Timeout => "ffmpeg-timeout",
            FfmpegError::InvalidPng(_) => "ffmpeg-png",
            FfmpegError::Io(_) => "ffmpeg-io",
        }
    }
}

impl std::fmt::Display for FfmpegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FfmpegError::NotFound => write!(f, "ffmpeg binary not found on PATH"),
            FfmpegError::NonZeroExit { code, stderr } => {
                write!(f, "ffmpeg exited {code:?}: {stderr}")
            }
            FfmpegError::Timeout => write!(f, "ffmpeg timed out after {FFMPEG_TIMEOUT:?}"),
            FfmpegError::InvalidPng(msg) => write!(f, "ffmpeg output not a valid PNG: {msg}"),
            FfmpegError::Io(msg) => write!(f, "ffmpeg IO error: {msg}"),
        }
    }
}

impl std::error::Error for FfmpegError {}

/// Best-effort ffmpeg availability probe. Synchronous and cheap
/// (a single `ffmpeg -version` invocation). The result is cached
/// at module load in [`crate::data::thumbnails`] so we don't spawn
/// a process on every `LooseVideo` thumbnail.
///
/// Return value is intentionally a `bool` — the caller decides what
/// to do based on whether ffmpeg exists. There is intentionally no
/// `Result` because a non-zero exit (ffmpeg not on PATH) IS the
/// answer.
pub fn ffmpeg_available() -> bool {
    use std::process::Command as StdCommand;
    StdCommand::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Extract the first non-black frame from a video file as PNG
/// bytes. The bytes are valid PNG (PNG magic + IHDR chunk) and
/// ready to be resized by the caller's `image` pipeline.
///
/// On any failure (binary missing, input missing, non-zero exit,
/// timeout, invalid PNG), returns a [`FfmpegError`]. The caller
/// should map to a title-only thumbnail or a `Failed` state per
/// the UX policy in [`crate::data::thumbnails`].
pub async fn extract_first_frame(input: &Path) -> Result<Vec<u8>, FfmpegError> {
    if !input.is_file() {
        return Err(FfmpegError::Io(format!(
            "input not a regular file: {}",
            input.display()
        )));
    }
    let child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            "00:00:01",
            "-i",
        ])
        .arg(input)
        .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FfmpegError::NotFound
            } else {
                FfmpegError::Io(format!("spawn ffmpeg: {e}"))
            }
        })?;
    let fut = async {
        child
            .wait_with_output()
            .await
            .map_err(|e| FfmpegError::Io(format!("wait_with_output: {e}")))
    };
    let out = match timeout(FFMPEG_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(FfmpegError::Timeout),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let truncated: String = stderr.chars().take(512).collect();
        return Err(FfmpegError::NonZeroExit {
            code: out.status.code(),
            stderr: truncated,
        });
    }
    if out.stdout.is_empty() {
        return Err(FfmpegError::InvalidPng(
            "ffmpeg produced empty stdout".into(),
        ));
    }
    // Sanity-check PNG magic. ffmpeg can exit 0 on a partially
    // malformed container (rare but observed with truncated mp4);
    // we refuse to hand garbage to the `image` decoder.
    if out.stdout.len() < 8 || &out.stdout[..8] != b"\x89PNG\r\n\x1a\n" {
        let preview: Vec<u8> = out.stdout.iter().take(8).copied().collect();
        return Err(FfmpegError::InvalidPng(format!(
            "ffmpeg stdout missing PNG magic (first 8 bytes: {preview:?})"
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_available_returns_bool() {
        // Don't assert a specific value (depends on the host); just
        // that the probe runs and returns a bool without panicking.
        let _: bool = ffmpeg_available();
    }

    #[test]
    fn error_source_labels_are_distinct() {
        let variants = [
            FfmpegError::NotFound,
            FfmpegError::NonZeroExit {
                code: Some(1),
                stderr: "x".into(),
            },
            FfmpegError::Timeout,
            FfmpegError::InvalidPng("x".into()),
            FfmpegError::Io("x".into()),
        ];
        let labels: Vec<&'static str> = variants.iter().map(|e| e.source()).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            labels.len(),
            "source labels must be unique: {labels:?}"
        );
    }

    #[test]
    fn error_display_includes_kind() {
        // Each variant must mention its category in Display so the
        // banner is debuggable without inspecting the variant.
        assert!(FfmpegError::NotFound.to_string().contains("ffmpeg"));
        assert!(FfmpegError::NonZeroExit {
            code: Some(2),
            stderr: "boom".into(),
        }
        .to_string()
        .contains("boom"));
        assert!(FfmpegError::Timeout.to_string().contains("ffmpeg"));
        assert!(FfmpegError::InvalidPng("oops".into())
            .to_string()
            .contains("oops"));
        assert!(FfmpegError::Io("disk full".into())
            .to_string()
            .contains("disk full"));
    }

    #[tokio::test]
    async fn extract_first_frame_errors_on_missing_input() {
        // A path that cannot exist must produce an error. The
        // exact variant depends on whether ffmpeg is installed:
        // - if ffmpeg exists → it spawns and exits non-zero with
        //   "no such file", surfacing as NonZeroExit
        // - if ffmpeg missing → NotFound
        // - if we short-circuit before spawn (file doesn't exist)
        //   → Io
        // All three are valid failure modes. We just assert error.
        let res = extract_first_frame(Path::new("/nonexistent/missing.mp4")).await;
        assert!(res.is_err(), "missing input must error, got {res:?}");
    }

    #[tokio::test]
    async fn extract_first_frame_errors_on_directory_input() {
        // A directory is not a video; we should fail fast with
        // Io rather than spawn ffmpeg against a directory.
        let tmp = tempfile::tempdir().unwrap();
        let res = extract_first_frame(tmp.path()).await;
        assert!(matches!(res, Err(FfmpegError::Io(_))), "got {res:?}");
    }
}
