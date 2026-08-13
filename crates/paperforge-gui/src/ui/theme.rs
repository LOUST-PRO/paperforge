//! Theme primitives: state → color mappings and CSS constants.
//!
//! The GUI ships without an external CSS file for now (Dioxus
//! 0.8-alpha asset pipeline is rough). Inline `style` strings are
//! preferred because they round-trip cleanly through hot reload
//! and don't require an asset bundler step.
//!
//! When the GUI grows past 3 panels we should consolidate the
//! inline styles into an `assets/style.css` loaded via
//! `document::Stylesheet { href: asset!("/assets/style.css") }`.
//! For PR 2 the inline approach is cheaper.

use paperforge_core::backend::BackendState;

/// Hex color string for a given backend state. Used for badges,
/// status indicators, and the connection dot.
pub fn state_color(state: BackendState) -> &'static str {
    match state {
        BackendState::Running => "#3fb950",    // green
        BackendState::Paused => "#d29922",     // amber
        BackendState::NotRunning => "#6e7681", // dim gray
    }
}

/// Connection-status color (banner + title bar dot).
#[allow(dead_code)] // consumed by ui/status.rs in PR 3+
pub fn connection_color(connected: bool) -> &'static str {
    if connected {
        "#3fb950"
    } else {
        "#f85149"
    }
}

/// Severity → banner background color.
#[allow(dead_code)] // consumed by ui/status.rs in PR 3+
pub fn severity_bg(severity: &super::super::error::Severity) -> &'static str {
    use super::super::error::Severity;
    match severity {
        Severity::Notice => "#5a4500", // dim amber
        Severity::Error => "#5a1f1f",  // dim red
    }
}

/// Common inline style fragments. Keep the values short — they
/// inline into every rendered element.
pub const FONT_STACK: &str = "system-ui, -apple-system, sans-serif";
pub const PANEL_PADDING: &str = "padding: 0.75rem 1rem;";
pub const PANEL_BORDER: &str = "border: 1px solid #30363d; border-radius: 6px;";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_color_covers_all_variants() {
        assert_eq!(state_color(BackendState::Running), "#3fb950");
        assert_eq!(state_color(BackendState::Paused), "#d29922");
        assert_eq!(state_color(BackendState::NotRunning), "#6e7681");
    }

    #[test]
    fn connection_color_is_green_when_connected() {
        assert!(connection_color(true).starts_with("#3f"));
        assert!(connection_color(false).starts_with("#f8"));
    }
}
