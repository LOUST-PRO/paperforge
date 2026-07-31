//! Keyboard input handling.
//!
//! The TUI uses crossterm for raw-mode terminal events. We translate
//! [`crossterm::event::KeyEvent`] into [`Action`]s and dispatch them
//! to the [`App`](crate::app::App). The pure `translate` function
//! is unit-tested without a real terminal.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Focus};

/// What the user asked for, after we translated their keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Move focus to the next panel (Tab / `2` / `3` / `4`).
    NextPanel,
    /// Move focus to the previous panel (Backtab).
    PrevPanel,
    /// Jump focus to a specific panel by index (`1`-`4`).
    JumpPanel(usize),
    /// Move selection up (`Up` / `k`).
    Up,
    /// Move selection down (`Down` / `j`).
    Down,
    /// Force refresh of the focused panel (`r`).
    Refresh,
    /// Quit (`q` / `Esc` / `Ctrl-C`).
    Quit,
    /// No-op (key not bound).
    Noop,
}

/// Translate a single key event into an [`Action`].
pub fn translate(key: KeyEvent) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Action::Quit,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
        (KeyCode::Tab, _) => Action::NextPanel,
        (KeyCode::BackTab, _) => Action::PrevPanel,
        (KeyCode::Char('1'), _) => Action::JumpPanel(0),
        (KeyCode::Char('2'), _) => Action::JumpPanel(1),
        (KeyCode::Char('3'), _) => Action::JumpPanel(2),
        (KeyCode::Char('4'), _) => Action::JumpPanel(3),
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => Action::Up,
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => Action::Down,
        (KeyCode::Char('r'), _) | (KeyCode::Char('R'), _) => Action::Refresh,
        _ => Action::Noop,
    }
}

/// Apply an [`Action`] to the [`App`]. Returns `true` if a UI
/// refresh was requested (used by the event loop to know when to
/// dispatch a fetch).
pub fn apply(app: &mut App, action: Action) -> bool {
    match action {
        Action::Quit => {
            app.quit();
            false
        }
        Action::NextPanel => {
            app.focus = app.focus.next();
            app.selection = 0;
            app.clamp_selection();
            true
        }
        Action::PrevPanel => {
            app.focus = app.focus.prev();
            app.selection = 0;
            app.clamp_selection();
            true
        }
        Action::JumpPanel(idx) => {
            app.focus = Focus::ALL.get(idx).copied().unwrap_or(Focus::Outputs);
            app.selection = 0;
            app.clamp_selection();
            true
        }
        Action::Up => {
            app.selection_up();
            true
        }
        Action::Down => {
            app.selection_down();
            true
        }
        Action::Refresh => {
            app.set_status("refresh requested");
            true
        }
        Action::Noop => false,
    }
}

/// Wait for the next crossterm event with a timeout. Returns
/// `None` on timeout (used as a tick).
pub fn poll_event(timeout: Duration) -> std::io::Result<Option<Event>> {
    if crossterm::event::poll(timeout)? {
        Ok(Some(crossterm::event::read()?))
    } else {
        Ok(None)
    }
}

/// Drain pending crossterm events, returning the latest key event
/// (if any). Useful for keeping the input buffer clean between
/// ticks.
pub fn latest_key() -> std::io::Result<Option<KeyEvent>> {
    let mut latest = None;
    while let Some(ev) = poll_event(Duration::from_millis(0))? {
        if let Event::Key(k) = ev {
            latest = Some(k);
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn quit_keys_translate_to_quit() {
        assert_eq!(
            translate(k(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(translate(k(KeyCode::Esc, KeyModifiers::NONE)), Action::Quit);
        assert_eq!(
            translate(k(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn number_keys_jump_to_panels() {
        assert_eq!(
            translate(k(KeyCode::Char('1'), KeyModifiers::NONE)),
            Action::JumpPanel(0)
        );
        assert_eq!(
            translate(k(KeyCode::Char('2'), KeyModifiers::NONE)),
            Action::JumpPanel(1)
        );
        assert_eq!(
            translate(k(KeyCode::Char('3'), KeyModifiers::NONE)),
            Action::JumpPanel(2)
        );
        assert_eq!(
            translate(k(KeyCode::Char('4'), KeyModifiers::NONE)),
            Action::JumpPanel(3)
        );
    }

    #[test]
    fn tab_and_backtab_cycle_focus() {
        assert_eq!(
            translate(k(KeyCode::Tab, KeyModifiers::NONE)),
            Action::NextPanel
        );
        assert_eq!(
            translate(k(KeyCode::BackTab, KeyModifiers::NONE)),
            Action::PrevPanel
        );
    }

    #[test]
    fn arrows_and_vim_keys_navigate() {
        assert_eq!(translate(k(KeyCode::Up, KeyModifiers::NONE)), Action::Up);
        assert_eq!(
            translate(k(KeyCode::Down, KeyModifiers::NONE)),
            Action::Down
        );
        assert_eq!(
            translate(k(KeyCode::Char('k'), KeyModifiers::NONE)),
            Action::Up
        );
        assert_eq!(
            translate(k(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::Down
        );
    }

    #[test]
    fn refresh_keys_translate() {
        assert_eq!(
            translate(k(KeyCode::Char('r'), KeyModifiers::NONE)),
            Action::Refresh
        );
        assert_eq!(
            translate(k(KeyCode::Char('R'), KeyModifiers::NONE)),
            Action::Refresh
        );
    }

    #[test]
    fn unbound_keys_translate_to_noop() {
        assert_eq!(
            translate(k(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Noop
        );
    }

    #[test]
    fn apply_quit_marks_app() {
        let mut app = App::new();
        let dirty = apply(&mut app, Action::Quit);
        assert!(!dirty);
        assert!(app.should_quit);
    }

    #[test]
    fn apply_jump_panel_resets_selection() {
        let mut app = App::new();
        app.selection = 5;
        let dirty = apply(&mut app, Action::JumpPanel(2));
        assert!(dirty);
        assert_eq!(app.focus, Focus::Playlists);
        assert_eq!(app.selection, 0, "selection must reset on panel jump");
    }

    #[test]
    fn apply_refresh_sets_status() {
        let mut app = App::new();
        let dirty = apply(&mut app, Action::Refresh);
        assert!(dirty);
        assert_eq!(app.status.as_deref(), Some("refresh requested"));
    }
}
