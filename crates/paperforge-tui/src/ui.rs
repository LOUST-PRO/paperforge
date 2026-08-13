//! Ratatui-based rendering for the TUI.
//!
//! Layout:
//!
//! ```text
//! ┌─ outputs (3) ─┬─ running (2) ─┬─ playlists (1) ─┐
//! │ DP-1          │ 1234 running  │ focus           │
//! │ eDP-1         │ 1235 paused   │                 │
//! └───────────────┴───────────────┴─────────────────┘
//! ┌─ inventory (124) ─────────────────────────────────┐
//! │ /scenes/forest  │ Workshop │ 2.1 MiB │ 2026-07-10 │
//! │ /scenes/cyber   │ Workshop │ 5.4 MiB │ 2026-07-12 │
//! │ /videos/aurora  │ Video    │ 12  MiB │ 2026-07-15 │
//! └───────────────────────────────────────────────────┘
//! 1-4 focus  ↑↓/jk nav  r refresh  q quit
//! ```
//!
//! All panels are rendered as [`Table`](ratatui::widgets::Table)s for
//! uniform styling. Each panel highlights its title when focused.

use paperforge_core::format::{
    entry_size_on_disk, format_entry_path, format_mtime_ago, format_size,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::{
    app::{App, Focus},
    data::{RunningInstance, Snapshot},
};

fn panel_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_outputs_panel(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Outputs;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" outputs ({}) ", app.snapshot.outputs.len()),
        panel_style(focused),
    ));
    let rows: Vec<Row> = app
        .snapshot
        .outputs
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let style = if focused && i == app.selection {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![Cell::from(o.name.clone())]).style(style)
        })
        .collect();
    let widths = [Constraint::Min(8)];
    let table = Table::new(rows, widths).block(block);
    f.render_widget(table, area);
}

fn render_running_panel(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Running;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" running ({}) ", app.snapshot.running.len()),
        panel_style(focused),
    ));
    let rows: Vec<Row> = app
        .snapshot
        .running
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let style = if focused && i == app.selection {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let state_cell = Cell::from(format!("{:?}", r.state))
                .style(Style::default().fg(state_color(&r.state)));
            Row::new(vec![Cell::from(r.pid.to_string()), state_cell]).style(style)
        })
        .collect();
    if rows.is_empty() {
        let p = Paragraph::new(" (no LWE PIDs running) ")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, area);
        return;
    }
    let widths = [Constraint::Length(8), Constraint::Min(10)];
    let table = Table::new(rows, widths).block(block);
    f.render_widget(table, area);
}

fn state_color(state: &paperforge_core::backend::BackendState) -> Color {
    use paperforge_core::backend::BackendState;
    match state {
        BackendState::Running => Color::Green,
        BackendState::Paused => Color::Yellow,
        BackendState::NotRunning => Color::DarkGray,
    }
}

fn render_playlists_panel(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Playlists;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" playlists ({}) ", app.snapshot.playlists.len()),
        panel_style(focused),
    ));
    if app.snapshot.playlists.is_empty() {
        let p = Paragraph::new(" (no playlists — use `paperforge playlist new`) ")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, area);
        return;
    }
    let rows: Vec<Row> = app
        .snapshot
        .playlists
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if focused && i == app.selection {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(format!("{} wp / {} out", p.wallpapers, p.outputs)),
            ])
            .style(style)
        })
        .collect();
    let widths = [Constraint::Min(8), Constraint::Length(18)];
    let table = Table::new(rows, widths).block(block);
    f.render_widget(table, area);
}

#[allow(dead_code)]
fn format_running_row(r: &RunningInstance) -> String {
    format!("{:>6}  {:?}", r.pid, r.state)
}

#[allow(dead_code)]
fn format_inventory_summary(s: &Snapshot) -> String {
    format!(
        "{} entries ({} LWE-compatible)",
        s.inventory.len(),
        s.lwe_compatible_count()
    )
}

#[allow(dead_code)]
fn inventory_row(idx: usize, app: &App) -> Option<Row<'static>> {
    let entry = app.snapshot.inventory.get(idx)?;
    let path = format_entry_path(&entry.path, 60);
    let size = format_size(entry_size_on_disk(&entry.path, entry.kind));
    let mtime = format_mtime_ago(entry.mtime);
    let kind = format!("{:?}", entry.kind);
    let focused = app.focus == Focus::Inventory && idx == app.selection;
    let style = if focused {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Some(
        Row::new(vec![
            Cell::from(path),
            Cell::from(kind),
            Cell::from(size),
            Cell::from(mtime),
        ])
        .style(style),
    )
}

/// Render the full TUI layout: title bar + 3 top panels (outputs /
/// running / playlists) + inventory full-width + status bar.
///
/// This is the entry point used by `paperforge-tui`'s main loop.
pub fn render_top_half_with_inventory(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title bar
            Constraint::Min(8),    // top row + inventory
            Constraint::Length(1), // status bar
        ])
        .split(area);

    // Title bar.
    let title = format!(
        " paperforge-tui v{} • uptime {}s • inventory={} (LWE-compat: {}) ",
        env!("CARGO_PKG_VERSION"),
        app.started_at.elapsed().as_secs(),
        app.snapshot.inventory_count(),
        app.snapshot.lwe_compatible_count(),
    );
    let title_p = Paragraph::new(title)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(title_p, chunks[0]);

    // Middle: top row of 3 panels + inventory full-width.
    let middle = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);

    let top_panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(middle[0]);

    render_outputs_panel(f, top_panels[0], app);
    render_running_panel(f, top_panels[1], app);
    render_playlists_panel(f, top_panels[2], app);

    render_inventory_panel(f, middle[1], app);

    // Status bar.
    let mut spans = vec![
        Span::styled("1", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("-"),
        Span::styled("4", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" focus  "),
        Span::styled("↑↓/jk", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" nav  "),
        Span::styled("r", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" refresh  "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ];
    if let Some(err) = app.snapshot.errors.last() {
        spans.push(Span::raw("  │ "));
        spans.push(Span::styled(
            format!("[{}] {}", err.source, err.message),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(status) = &app.status {
        spans.push(Span::raw("  │ "));
        spans.push(Span::styled(
            status.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    let status_p = Paragraph::new(ratatui::text::Line::from(spans));
    f.render_widget(status_p, chunks[2]);
}

fn render_inventory_panel(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Inventory;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(
            " inventory ({}/{} LWE-compatible) ",
            app.snapshot.lwe_compatible_count(),
            app.snapshot.inventory.len()
        ),
        panel_style(focused),
    ));
    if app.snapshot.inventory.is_empty() {
        let p = Paragraph::new(" (no wallpapers found — check source roots) ")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, area);
        return;
    }
    let rows: Vec<Row> = app
        .snapshot
        .inventory
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let path = format_entry_path(&entry.path, 60);
            let size = format_size(entry_size_on_disk(&entry.path, entry.kind));
            let mtime_str = format_mtime_ago(entry.mtime);
            let kind = format!("{:?}", entry.kind);
            let style = if focused && i == app.selection {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(path),
                Cell::from(kind),
                Cell::from(size),
                Cell::from(mtime_str),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Min(40),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths).block(block);
    f.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mtime_ago_returns_iso_date() {
        use std::time::{Duration, UNIX_EPOCH};
        let mtime = UNIX_EPOCH + Duration::from_secs(86400 * 30);
        let s = format_mtime_ago(mtime);
        assert_eq!(s, "1970-01-31");
    }

    #[test]
    fn inventory_row_returns_none_for_out_of_bounds() {
        let app = App::new();
        assert!(inventory_row(0, &app).is_none());
    }

    #[test]
    fn inventory_row_builds_row_when_present() {
        let mut app = App::new();
        use paperforge_core::inventory::{WallpaperEntry, WallpaperKind};
        use std::path::PathBuf;
        app.snapshot.inventory.push(WallpaperEntry {
            path: PathBuf::from("/scenes/forest"),
            mtime: std::time::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("Forest".to_string()),
            workshop_id: None,
        });
        // Build the row and render it into a TestBackend to extract
        // its cell text. ratatui 0.29 doesn't expose Row cells as a
        // public iterator, so we go through the render path.
        let backend = ratatui::backend::TestBackend::new(120, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let row = inventory_row(0, &app).expect("row at idx 0");
                let table = ratatui::widgets::Table::new(
                    vec![row],
                    [
                        ratatui::layout::Constraint::Min(40),
                        ratatui::layout::Constraint::Length(12),
                        ratatui::layout::Constraint::Length(10),
                        ratatui::layout::Constraint::Length(12),
                    ],
                );
                f.render_widget(table, f.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let line: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            line.contains("forest"),
            "expected 'forest' in rendered row, got: {line:?}"
        );
        assert!(
            line.contains("Workshop"),
            "expected kind label in rendered row"
        );
    }

    #[test]
    fn format_running_row_includes_pid_and_state() {
        use paperforge_core::backend::BackendState;
        let r = RunningInstance {
            pid: 1234,
            state: BackendState::Running,
        };
        let s = format_running_row(&r);
        assert!(s.contains("1234"));
        assert!(s.contains("Running"));
    }
}
