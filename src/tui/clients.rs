use super::{
    AppState, Cell, Constraint, DetailTab, Feeds, HighlightSpacing, KeyCode, KeyEvent,
    KeyEventKind, Modifier, OverviewModal, PaneCtx, PollerRequest, Rect, Result, Row, TableState,
    base_style, draw_text_popup, has_command_modifier, scroll_popup, selection_gutter_cell,
    truncate_chars,
};

pub(super) fn sync_target(app: &mut AppState, feeds: &Feeds) -> Result<()> {
    let keys = if app.is_detail_tab(DetailTab::Clients) {
        app.action_target_keys()
    } else {
        Vec::new()
    };
    let targets = keys
        .iter()
        .map(|key| (key.clone(), app.server_label(key)))
        .collect::<std::collections::BTreeMap<_, _>>();
    if !app.clients.targets.keys().eq(targets.keys()) {
        feeds.send_required(PollerRequest::WatchClients { keys }, "watch clients")?;
    }
    app.clients.set_targets(targets);
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
                if let Some(targets) = &app.clients.confirmation {
                    feeds.send_required(
                        PollerRequest::KillClients {
                            targets: targets.clone(),
                        },
                        "kill clients",
                    )?;
                }
                app.clients.result.clear();
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
    let table_area = draw_errors(frame, app, area);
    app.clients.page_len = usize::from(table_area.height.saturating_sub(3)).max(1);
    let pane = PaneCtx {
        app,
        tab: DetailTab::Clients,
        area: table_area,
    };
    let rows = app.clients.visible(&filter);
    let aggregate = app.clients.targets.len() > 1;
    let loading = app.clients.targets.len() > app.clients.own_ids.len() + app.clients.errors.len();
    let title = list_title(app, &filter, rows.len());
    let block = pane.block(title);
    if rows.is_empty() {
        let message = if loading {
            "Loading clients…"
        } else {
            "No clients match the current filter"
        }
        .into();
        pane.message(frame, block, message);
        return;
    }
    let columns: Vec<_> = app.clients.columns.visible_in_order().collect();
    let alias_width = app
        .clients
        .targets
        .values()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(5, 32);
    let widths = std::iter::once(Constraint::Length(1))
        .chain(aggregate.then_some(Constraint::Length(u16::try_from(alias_width).unwrap_or(32))))
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
        .chain(aggregate.then_some(Cell::from("Alias")))
        .chain(columns.iter().map(|key| Cell::from(key.to_uppercase())));
    let table_rows =
        rows.iter()
            .map(|row| {
                let own = app.clients.is_own(row);
                let gutter = selection_gutter_cell(
                    app,
                    app.clients.focus.as_ref() == Some(&row.key()),
                    app.clients.marked.contains(&row.key()),
                );
                let cells = std::iter::once(gutter)
                    .chain(aggregate.then(|| {
                        Cell::from(truncate_chars(app.clients.alias(&row.server), 32, "…"))
                    }))
                    .chain(columns.iter().map(|key| {
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
        .position(|row| app.clients.focus.as_ref() == Some(&row.key()));
    let mut state = TableState::default().with_selected(index);
    let table = pane
        .table(table_rows, widths, header, block)
        .highlight_spacing(HighlightSpacing::Never);
    frame.render_stateful_widget(table, table_area, &mut state);
}

fn draw_errors(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) -> Rect {
    if app.clients.errors.is_empty() {
        area
    } else {
        let errors = app
            .clients
            .errors
            .iter()
            .map(|(key, error)| format!("{}: {error}", app.clients.alias(key)))
            .collect::<Vec<_>>()
            .join("\n");
        let [error_area, remaining] = super::Layout::vertical([
            Constraint::Length(u16::try_from(app.clients.errors.len()).unwrap_or(3).min(3)),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(
            super::Paragraph::new(errors).style(base_style(app)),
            error_area,
        );
        remaining
    }
}

fn list_title(app: &AppState, filter: &str, visible: usize) -> String {
    let aggregate = app.clients.targets.len() > 1;
    let scope = if aggregate {
        format!("Clients across {} servers", app.clients.targets.len())
    } else {
        "Clients".into()
    };
    let own = if aggregate {
        "italic".into()
    } else {
        app.clients
            .own_ids
            .values()
            .next()
            .map_or_else(|| "-".into(), u64::to_string)
    };
    let loading = app.clients.targets.len() - app.clients.own_ids.len() - app.clients.errors.len();
    format!(
        "{scope} {} / {}  selected={}  self={own}  errors={} loading={loading}  filter=/{filter}",
        visible,
        app.clients.rows.len(),
        app.clients.marked.len(),
        app.clients.errors.len()
    )
}

pub(super) fn draw_popup(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    let (title, text) = if app.overview_modal == OverviewModal::ClientKillConfirmation {
        let targets = app
            .clients
            .confirmation
            .as_ref()
            .map(|targets| {
                targets
                    .iter()
                    .map(|(key, ids)| {
                        format!(
                            "{} ({key}): clients {}",
                            app.clients.alias(key),
                            ids.iter()
                                .map(u64::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        (
            "Confirm client kill",
            format!(
                "{}\n{targets}\n\ny confirms; Enter/n/Esc cancels.",
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

    fn client_key(id: u64) -> crate::clients::ClientKey {
        crate::clients::ClientKey {
            server: "server".into(),
            id,
        }
    }

    fn app() -> AppState {
        let settings = crate::config::default_settings();
        let registry = ColumnRegistry::load(None, true, settings.default_sort);
        let mut app = AppState::new(settings, registry);
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Clients;
        app.clients.set_targets(std::collections::BTreeMap::from([(
            "server".into(),
            "server".into(),
        )]));
        app.clients.apply("server", Ok(crate::clients::ClientSnapshot {
            rows: crate::clients::parse_list("server", "id=42 addr=localhost name=alpha user=default\nid=43 addr=localhost name=beta user=default").unwrap(),
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
        app.clients.focus = Some(client_key(43));
        handle_key(&mut app, key(KeyCode::Char('y')), &feeds).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::KillClients { targets } if targets == std::collections::BTreeMap::from([("server".into(), vec![42])]))
        );
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn aggregate_scope_includes_hidden_selections_and_uses_overview_aliases() {
        let mut app = app();
        let keys: Vec<String> = ["127.0.0.1:6379", "127.0.0.1:6380"]
            .map(str::to_owned)
            .into();
        for key in &keys {
            app.apply_update(crate::model::InstanceState::new(key.clone(), key.clone()));
        }
        app.select_server_range(&keys);
        app.filter = "no overview matches".into();
        let (_, updates_rx) = tokio::sync::mpsc::channel(1);
        let (_, discovery_rx) = tokio::sync::mpsc::channel(1);
        let (request_tx, mut requests) = tokio::sync::mpsc::channel(4);
        let feeds = Feeds {
            updates_rx,
            discovery_rx,
            request_tx,
        };
        sync_target(&mut app, &feeds).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::WatchClients { keys: watched } if watched == keys)
        );
        assert_eq!(app.clients.alias(&keys[0]), "6379");
        assert_eq!(app.clients.alias(&keys[1]), "6380");
        for key in &keys {
            app.clients.apply(
                key,
                Ok(crate::clients::ClientSnapshot {
                    rows: crate::clients::parse_list(key, "id=42 name=worker").unwrap(),
                    own_id: 99,
                }),
                "",
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 16)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, &mut app))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for expected in [
            "Clients across 2 selected servers",
            "Clients across 2 servers",
            "Alias",
            "6379",
            "6380",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
        app.clients.select_all(true);
        app.clients.ask_kill("");
        app.overview_modal = OverviewModal::ClientKillConfirmation;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &feeds,
        )
        .unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::KillClients { targets } if targets == keys.iter().map(|key| (key.clone(), vec![42])).collect())
        );
        app.instances.get_mut(&keys[1]).unwrap().alias = Some("cache".into());
        sync_target(&mut app, &feeds).unwrap();
        assert_eq!(app.clients.alias(&keys[1]), "cache");
        assert!(requests.try_recv().is_err());
        app.clients
            .apply(&keys[0], Err("permission denied".into()), "");
        terminal
            .draw(|frame| draw(frame, &mut app, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("6379: permission denied"));
        assert!(text.contains("cache"));
        assert!(text.contains("worker"));
        app.filter.clear();
        app.clear_server_selection();
        sync_target(&mut app, &feeds).unwrap();
        assert_eq!(app.clients.targets.len(), 1);
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::WatchClients { keys } if keys.len() == 1)
        );
    }

    #[test]
    fn batch_results_keep_each_server_and_its_outcome() {
        let mut app = app();
        let (updates, updates_rx) = tokio::sync::mpsc::channel(2);
        let (_, discovery_rx) = tokio::sync::mpsc::channel(1);
        let (request_tx, _) = tokio::sync::mpsc::channel(1);
        let mut feeds = Feeds {
            updates_rx,
            discovery_rx,
            request_tx,
        };
        for (key, outcome) in [("a", Ok(1)), ("b", Err("permission denied".into()))] {
            updates
                .try_send(crate::poller::PollerUpdate::ClientsKilled {
                    key: key.into(),
                    results: vec![(42, outcome)],
                })
                .unwrap();
        }
        feeds.drain_into(&mut app);
        for expected in [
            "Server: a",
            "Client 42: killed",
            "Server: b",
            "Client 42: failed: permission denied",
        ] {
            assert!(app.clients.result.contains(expected), "{expected}");
        }
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
            app.clients.focus = Some(client_key(if focused { 42 } else { 43 }));
            app.clients.marked.clear();
            if selected {
                app.clients.marked.insert(client_key(42));
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
        app.clients.marked.insert(client_key(42));
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
