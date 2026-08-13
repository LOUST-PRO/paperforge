//! Outputs panel — local scan via `CompositorHotplugSource`.
//!
//! The GUI consumes the same `CompositorHotplugSource` as the TUI
//! (`swaymsg -t get_outputs` / `hyprctl monitors -j` via the binary
//! the operator has installed). The full hotplug watcher (with diff
//! detection) lives in `paperforge-core::hotplug`; for PR 3 we just
//! fetch the current snapshot every 2s.
//!
//! Why no full watcher here: the watcher owns a `tokio::spawn` task
//! that lives for the GUI's lifetime and requires `use_coroutine`
//! wiring in `app.rs`. PR 3 keeps the data layer stateless; the loop
//! that calls `refresh_outputs` lives in `ui::root`. PR 4 adds the
//! watcher once we have a single "scan everything once at startup"
//! cadence.

use std::sync::Arc;

use paperforge_core::hotplug::{CompositorHotplugSource, HotplugSource, Output};

use crate::error::GuiError;

/// Fetch the current set of Wayland outputs from the compositor.
///
/// Returns `(outputs, error)`:
/// - On success: `(Vec<Output>, None)`.
/// - On failure: `(Vec::new(), Some(GuiError))` — the caller keeps
///   the previous snapshot (keep-stale-data UX policy) and surfaces
///   the error via the status banner.
///
/// The `CompositorHotplugSource::list_outputs` call is async-friendly
/// (it spawns a subprocess internally and returns a future), so we
/// do NOT wrap it in `spawn_blocking` — the call is short and
/// doesn't touch the local filesystem.
#[allow(dead_code)] // consumed by ui/root.rs in PR 3 coroutine
pub async fn refresh_outputs(src: Arc<CompositorHotplugSource>) -> (Vec<Output>, Option<GuiError>) {
    match src.list_outputs().await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(GuiError::Core(format!("[outputs] {e}")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_outputs_unavailable_source_returns_empty_no_error() {
        // `CompositorHotplugSource::detect()` returns a source with
        // no override_cmd when no compositor CLI is found in PATH
        // (which is the case in CI). The trait impl returns
        // `Ok(Vec::new())` for that state — a "nothing configured"
        // situation, NOT an error.
        let src = Arc::new(CompositorHotplugSource::detect());
        let (v, err) = refresh_outputs(src).await;
        // Either we successfully detected (e.g. in CI's PATH) and
        // got an empty Vec, or detection failed but returned empty
        // (the trait implementation always returns Ok for missing
        // override_cmd). Either way, no error variant.
        assert!(
            err.is_none(),
            "missing compositor CLI must not error: {err:?}"
        );
        let _ = v; // size unspecified — depends on the host's PATH
    }
}
