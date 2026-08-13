//! Inventory panel — local filesystem scan via `Inventory::scan`.
//!
//! PR 3 only refreshes the inventory every 30s (the operator rarely
//! adds new wallpapers). Per-file thumbnail decode is PR 8.
//!
//! Why `spawn_blocking`: `Inventory::scan` walks the Workshop tree
//! depth-4 and reads `project.json` for every scene. With a few
//! hundred scenes that's tens of milliseconds — not enough to stall
//! the render loop, but enough to push us into spawn_blocking if
//! the operator has thousands of wallpapers.

use std::path::PathBuf;

use paperforge_core::error::Error as CoreError;
use paperforge_core::inventory::{Inventory, WallpaperEntry};

use crate::error::GuiError;

/// Fetch the wallpaper inventory across multiple source roots.
///
/// Walks each root at depth 4 (covers
/// `~/<source>/<author>/<item>/project.json`).
///
/// Returns `(entries, error)`:
/// - On success: `(Vec<WallpaperEntry>, None)`.
/// - On failure: `(Vec::new(), Some(GuiError::Core))`. The caller
///   keeps the previous snapshot (keep-stale-data UX policy).
///
/// Missing roots are silently skipped (matches the TUI's behavior —
/// a freshly-installed operator may have no Workshop content yet).
#[allow(dead_code)] // consumed by ui/root.rs in PR 3 coroutine
pub async fn refresh_inventory(roots: Vec<PathBuf>) -> (Vec<WallpaperEntry>, Option<GuiError>) {
    let join = tokio::task::spawn_blocking(
        move || -> std::result::Result<Vec<WallpaperEntry>, CoreError> {
            let mut inventory = Inventory::new();
            for root in &roots {
                if root.exists() {
                    // Per-root scan errors are non-fatal: a malformed
                    // `project.json` should not hide the rest of the
                    // inventory. `Inventory::scan` already swallows
                    // per-file errors and logs them internally.
                    let _ = inventory.scan(root, 4);
                }
            }
            Ok(inventory.entries().cloned().collect())
        },
    )
    .await;

    let result: std::result::Result<Vec<WallpaperEntry>, CoreError> = match join {
        Err(join_err) => Err(CoreError::Other(anyhow::anyhow!(
            "spawn_blocking (inventory): {join_err}"
        ))),
        Ok(inner) => inner,
    };

    match result {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(GuiError::from_core(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_inventory_on_empty_roots_returns_empty() {
        let (v, err) = refresh_inventory(Vec::new()).await;
        assert!(err.is_none());
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn refresh_inventory_on_nonexistent_root_returns_empty() {
        // tmpdir()'s child doesn't exist — refresh_inventory should
        // silently skip non-existent roots and return Ok(empty).
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let (v, err) = refresh_inventory(vec![missing]).await;
        assert!(err.is_none(), "missing root must not error: {err:?}");
        assert!(v.is_empty());
    }
}
