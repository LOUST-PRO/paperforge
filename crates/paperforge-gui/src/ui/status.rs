//! Status banner — keep-stale-data error display.
//!
//! PR 3 shows the highest-priority `GuiError` from the most recent
//! panel refresh. Errors clear when the next refresh succeeds.
//!
//! Severity colors come from `theme::severity_bg`. The dismiss
//! button is a small ✕ — clicking it sets the banner signal to
//! `None`. PR 4 adds auto-dismiss timers for `Notice` variants.

use dioxus::prelude::*;

use crate::error::{GuiError, Severity};
use crate::ui::theme::severity_bg;

/// Render the status banner. Returns an empty `Element` when no
/// error is present so the parent layout doesn't reserve space.
#[allow(non_snake_case)]
#[component]
pub fn StatusBanner(error: Option<GuiError>, on_dismiss: EventHandler<()>) -> Element {
    let Some(err) = error else {
        return rsx! { div { style: "display: none;" } };
    };
    let bg = severity_bg(&err.severity());
    rsx! {
        div {
            style: "background: {bg}; color: #ffdcd7; padding: 0.5rem 1rem; border-radius: 4px; margin-bottom: 0.75rem; display: flex; align-items: center; gap: 0.75rem;",
            span {
                style: "font-family: monospace; font-size: 0.75rem; color: #ffd7a8; padding: 0.125rem 0.5rem; background: rgba(0,0,0,0.3); border-radius: 3px;",
                "{err.source()}"
            }
            span {
                style: "flex: 1; font-size: 0.875rem;",
                "{err}"
            }
            button {
                style: "background: transparent; border: 1px solid rgba(255,255,255,0.3); color: inherit; padding: 0.125rem 0.5rem; border-radius: 3px; cursor: pointer; font-size: 0.75rem;",
                onclick: move |_| on_dismiss.call(()),
                "✕"
            }
        }
    }
}

#[allow(dead_code)] // helper used by tests + future PRs
pub fn is_notice(err: &GuiError) -> bool {
    matches!(err.severity(), Severity::Notice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_notice_only_true_for_notice_variant() {
        let notice = GuiError::Notice("hello".into());
        let error = GuiError::Core("core".into());
        let io = GuiError::Io("io".into());
        let ipc = GuiError::Ipc {
            kind: "x",
            message: "y".into(),
        };
        assert!(is_notice(&notice));
        assert!(!is_notice(&error));
        assert!(!is_notice(&io));
        assert!(!is_notice(&ipc));
    }
}
