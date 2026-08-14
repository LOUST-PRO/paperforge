//! Integration tests for `paperforge-gui::data::playlists`.
//!
//! These exercise the full on-disk round-trip (write via the
//! helper, read back via `PlaylistStore::load`). Lives in `tests/`
//! rather than the unit-test slot because it needs a `tempfile`
//! fixture and the API is stable enough to test through the
//! public surface.
//!
//! The unit tests in `data/playlists.rs` cover the `refresh_*`
//! helpers without touching the disk; this file covers the
//! write+read round-trip path.

use std::path::PathBuf;

use paperforge_core::playlist::{Playlist, PlaylistStore};
use paperforge_gui::data::playlists;

#[tokio::test]
async fn save_playlist_overwrites_and_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let mut pl = Playlist {
        name: "roundtrip".into(),
        description: None,
        outputs: vec!["DP-1".into()],
        wallpapers: vec![PathBuf::from("/tmp/wp1")],
        fill: paperforge_core::playlist::FillMode::Fill,
    };

    playlists::save_playlist(tmp.path().to_path_buf(), pl.clone())
        .await
        .expect("first save");
    let loaded = PlaylistStore::new(tmp.path())
        .unwrap()
        .load("roundtrip")
        .unwrap();
    assert_eq!(loaded, pl, "first save should round-trip cleanly");

    // Mutate and re-save via the helper. The store must overwrite
    // — saving the same name twice should NOT create a duplicate
    // or append.
    pl.wallpapers.push(PathBuf::from("/tmp/wp2"));
    pl.description = Some("updated".into());
    playlists::save_playlist(tmp.path().to_path_buf(), pl.clone())
        .await
        .expect("second save");
    let reloaded = PlaylistStore::new(tmp.path())
        .unwrap()
        .load("roundtrip")
        .unwrap();
    assert_eq!(reloaded, pl, "second save should overwrite without duplicates");

    // Verify the file count is exactly one (no sharding or
    // accidental concat). `store.list()` returns the sorted stems.
    let names = PlaylistStore::new(tmp.path()).unwrap().list().unwrap();
    assert_eq!(names, vec!["roundtrip".to_string()]);
}

#[tokio::test]
async fn save_playlist_creates_store_directory_if_missing() {
    // Spawn_blocking path inside `save_playlist` calls
    // `PlaylistStore::new`, which creates the root directory if
    // it doesn't exist. We exercise that branch by passing a
    // non-existent subdir.
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("nested").join("playlists");
    assert!(!nested.exists(), "precondition: nested dir absent");

    let pl = Playlist {
        name: "first".into(),
        description: None,
        outputs: vec!["eDP-1".into()],
        wallpapers: vec![PathBuf::from("/tmp/wp1")],
        fill: paperforge_core::playlist::FillMode::Fill,
    };
    playlists::save_playlist(nested.clone(), pl.clone())
        .await
        .expect("save into fresh nested dir");
    assert!(nested.is_dir(), "store root should be created");
    let loaded = PlaylistStore::new(&nested)
        .unwrap()
        .load("first")
        .unwrap();
    assert_eq!(loaded, pl);
}
