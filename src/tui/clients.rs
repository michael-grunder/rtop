use super::{
    AppState, Cell, Constraint, DetailTab, Feeds, HighlightSpacing, KeyCode, KeyEvent,
    KeyEventKind, Modifier, OverviewModal, PaneCtx, PollerRequest, Rect, Result, Row, TableState,
    base_style, draw_text_popup, has_command_modifier, scroll_popup, selection_gutter_cell,
    truncate_chars,
};

pub(super) fn sync_target(app: &mut AppState, feeds: &Feeds) -> Result<()> {
    let target = app
        .is_detail_tab(DetailTab::Clients)
        .then(|| app.selected_key())
        .flatten();
    if app.clients.target != target {
        feeds.send_required(
            PollerRequest::WatchClients {
                key: target.clone(),
            },
            "watch clients",
        )?;
        app.clients.set_target(target);
    }
    Ok(())
}

pub(super) fn handle_key(app: &mut AppState, key: KeyEvent, feeds: &Feeds) -> Result<bool> {
    if matches!(
        app.overview_modal,
        OverviewModal::ClientKillConfirmation | OverviewModal::ClientKillResult
    ) {
        scroll_popup(app, key);
        if key.kind == KeyEventKind::Press && !has_command_modifier(key) {
            if app.overview_modal == OverviewModal::ClientKillConfirmation
                && matches!(key.code, KeyCode::Char('y' | 'Y'))
            {
                if let Some((target, ids)) = &app.clients.confirmation {
                    feeds.send_required(
                        PollerRequest::KillClients {
                            key: target.clone(),
                            ids: ids.clone(),
                        },
                        "kill clients",
                    )?;
                }
                app.close_overview_modal();
            } else if matches!(key.code, KeyCode::Enter | KeyCode::Char('n' | 'N')) {
                app.close_overview_modal();
            }
        }
        return Ok(true);
    }
    if !app.is_detail_tab(DetailTab::Clients)
        || app.overview_modal != OverviewModal::None
        || key.kind != KeyEventKind::Press
        || has_command_modifier(key)
    {
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('c' | 'C' | 'v' | 'V') | KeyCode::F(7) => app.open_column_picker(),
        KeyCode::Char('x' | 'X' | 'K') | KeyCode::F(9) => {
            app.clients
                .ask_kill(&app.pane(DetailTab::Clients).filter.clone());
            if app.clients.confirmation.is_some() {
                app.popup_scroll.to_start();
                app.overview_modal = OverviewModal::ClientKillConfirmation;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    let filter = app.pane(DetailTab::Clients).filter.clone();
    app.clients.move_focus(&filter, 0);
    app.clients.page_len = usize::from(area.height.saturating_sub(3)).max(1);
    let pane = PaneCtx {
        app,
        tab: DetailTab::Clients,
        area,
    };
    let rows = app.clients.visible(&filter);
    let title = format!(
        "Clients {} / {}  selected={}  self={}  filter=/{filter}",
        rows.len(),
        app.clients.rows.len(),
        app.clients.marked.len(),
        app.clients
            .own_id
            .map_or_else(|| "-".into(), |id| id.to_string())
    );
    let block = pane.block(title);
    if rows.is_empty() {
        let message = app.clients.error.clone().unwrap_or_else(|| {
            if app.clients.own_id.is_none() {
                "Loading clients…".into()
            } else {
                "No clients match the current filter".into()
            }
        });
        pane.message(frame, block, message);
        return;
    }
    let columns: Vec<_> = app.clients.columns.visible_in_order().collect();
    let widths = std::iter::once(Constraint::Length(1))
        .chain(columns.iter().map(|key| {
            let width = rows
                .iter()
                .filter_map(|row| row.fields.get(*key))
                .map(|value| value.chars().count())
                .max()
                .unwrap_or(0)
                .max(key.len())
                .clamp(4, 32);
            Constraint::Length(u16::try_from(width).unwrap_or(32))
        }))
        .collect::<Vec<_>>();
    let header = std::iter::once(Cell::from(""))
        .chain(columns.iter().map(|key| Cell::from(key.to_uppercase())));
    let table_rows = rows
        .iter()
        .map(|row| {
            let own = Some(row.id) == app.clients.own_id;
            let gutter = selection_gutter_cell(
                app,
                Some(row.id) == app.clients.focus,
                app.clients.marked.contains(&row.id),
            );
            let cells = std::iter::once(gutter).chain(columns.iter().map(|key| {
                let value = row.fields.get(*key).map_or("-", String::as_str);
                Cell::from(truncate_chars(value, 32, "…"))
            }));
            let row = Row::new(cells);
            if own {
                row.style(base_style(app).add_modifier(Modifier::ITALIC))
            } else {
                row
            }
        })
        .collect();
    let index = rows
        .iter()
        .position(|row| Some(row.id) == app.clients.focus);
    let mut state = TableState::default().with_selected(index);
    let table = pane
        .table(table_rows, widths, header, block)
        .highlight_spacing(HighlightSpacing::Never);
    frame.render_stateful_widget(table, area, &mut state);
}

pub(super) fn draw_popup(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    let (title, text) = if app.overview_modal == OverviewModal::ClientKillConfirmation {
        let target = app
            .clients
            .confirmation
            .as_ref()
            .map_or("", |(key, _)| key.as_str());
        (
            "Confirm client kill",
            format!(
                "{}\nServer: {target}\n\ny confirms; Enter/n/Esc cancels.",
                app.clients.kill_prompt()
            ),
        )
    } else {
        (
            "Client kill results",
            format!(
                "{}\n\nEnter/Esc/q closes; ↑↓/PgUp/PgDn scroll.",
                app.clients.result
            ),
        )
    };
    draw_text_popup(frame, app, area, title.into(), text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ActiveView;
    use crate::registry::ColumnRegistry;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> AppState {
        let settings = crate::config::default_settings();
        let registry = ColumnRegistry::load(None, true, settings.default_sort);
        let mut app = AppState::new(settings, registry);
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Clients;
        app.clients.set_target(Some("server".into()));
        app.clients.apply(Ok(crate::clients::ClientSnapshot {
            rows: crate::clients::parse_list("id=42 addr=localhost name=alpha user=default\nid=43 addr=localhost name=beta user=default").unwrap(),
            own_id: 43,
        }), "");
        app
    }

    #[test]
    fn confirmation_freezes_ids_requires_yes_and_reports_results() {
        let mut app = app();
        let (_, updates_rx) = tokio::sync::mpsc::channel(1);
        let (_, discovery_rx) = tokio::sync::mpsc::channel(1);
        let (request_tx, mut requests) = tokio::sync::mpsc::channel(2);
        let feeds = Feeds {
            updates_rx,
            discovery_rx,
            request_tx,
        };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        handle_key(&mut app, key(KeyCode::Char('x')), &feeds).unwrap();
        assert_eq!(app.clients.kill_prompt(), "Kill client 42?");
        handle_key(&mut app, key(KeyCode::Enter), &feeds).unwrap();
        assert!(requests.try_recv().is_err());
        handle_key(&mut app, key(KeyCode::Char('x')), &feeds).unwrap();
        app.clients.focus = Some(43);
        handle_key(&mut app, key(KeyCode::Char('y')), &feeds).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::KillClients { key, ids } if key == "server" && ids == vec![42])
        );
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn selection_gutter_matches_overview_states_style_and_width() {
        let mut app = app();
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        for (focused, selected, marker) in [
            (false, false, " "),
            (false, true, "●"),
            (true, false, ">"),
            (true, true, "▶"),
        ] {
            app.clients.focus = Some(if focused { 42 } else { 43 });
            app.clients.marked.clear();
            if selected {
                app.clients.marked.insert(42);
            }
            terminal
                .draw(|frame| draw(frame, &mut app, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let gutter = &buffer[(1, 2)];
            assert_eq!(gutter.symbol(), marker);
            assert_eq!(gutter.modifier.contains(Modifier::BOLD), focused);
            assert_eq!(
                gutter.fg,
                if focused {
                    super::super::carat_color(&app)
                } else {
                    super::super::foreground_color(&app)
                }
            );
            // One gutter column and one gap put the ID directly at column 3.
            assert_eq!(buffer[(3, 2)].symbol(), "4");
            assert!(!buffer[(3, 2)].modifier.contains(Modifier::BOLD));
            assert!(buffer[(3, 3)].modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn table_gutter_filter_and_column_picker_work_at_small_sizes() {
        let mut app = app();
        app.clients.marked.insert(42);
        app.open_column_picker();
        let index = app
            .column_picker_entries()
            .iter()
            .position(|entry| entry.label == "addr")
            .unwrap();
        app.column_picker_index = index;
        app.toggle_selected_column_visibility();
        assert!(!app.clients.columns.is_visible(&"addr".into()));
        app.move_selected_column(-1);
        app.close_overview_modal();
        app.pane_mut(DetailTab::Clients).filter = "alp".into();
        for (width, height) in [(120, 16), (30, 6), (8, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, &mut app, frame.area()))
                .unwrap();
            if width == 120 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect();
                assert!(text.contains("▶"));
                assert!(text.contains("alpha"));
                assert!(!text.contains("beta"));
                assert!(text.contains("self=43"));
            }
        }
    }
}
