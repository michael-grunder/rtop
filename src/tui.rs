use std::fmt::Write as _;
use std::io::{self, Stdout, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    ModifierKeyCode, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::app::{ActiveView, AppState, AuthField, FilterPromptMode, OverviewModal};
use crate::cli::{LaunchConfig, OutputMode};
use crate::column::EmphasisStyle;
use crate::discovery::{self, DiscoveryEvent};
use crate::hotkeys::{HotkeysMetric, HotkeysStatus};
use crate::model::KillAction;
use crate::overview::{
    ClusterGutterColor, fit_cell_text, render_plain_text, sort_direction_symbol, sortable_header,
};
use crate::poller::{self, PollerRequest, PollerUpdate};
use crate::registry::ColumnRegistry;
use crate::target_addr::tcp_host;

struct DetailTabSpec {
    title: &'static str,
    shortcut: char,
}

const DETAIL_TABS: [DetailTabSpec; 6] = [
    DetailTabSpec {
        title: "Summary",
        shortcut: 's',
    },
    DetailTabSpec {
        title: "Latency",
        shortcut: 'l',
    },
    DetailTabSpec {
        title: "Info Raw",
        shortcut: 'i',
    },
    DetailTabSpec {
        title: "Commandstats",
        shortcut: 'c',
    },
    DetailTabSpec {
        title: "Bigkeys",
        shortcut: 'b',
    },
    DetailTabSpec {
        title: "Hotkeys",
        shortcut: 'k',
    },
];

pub async fn run(launch: LaunchConfig) -> Result<()> {
    if launch.verbose {
        eprintln!(
            "redis-top: targets={} refresh={} connect_timeout={} command_timeout={}",
            launch.targets.len(),
            humantime::format_duration(launch.settings.refresh_interval),
            humantime::format_duration(launch.settings.connect_timeout),
            humantime::format_duration(launch.settings.command_timeout)
        );
    }
    if launch.once {
        return run_once(launch).await;
    }
    if launch.output_mode == OutputMode::Json {
        return run_json_stream(launch).await;
    }
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, launch);
    restore_terminal(&mut terminal)?;
    result
}

async fn run_once(launch: LaunchConfig) -> Result<()> {
    let registry = ColumnRegistry::load(
        launch.config_path.as_deref(),
        launch.no_default_config,
        launch.settings.default_sort,
    );
    let mut app = AppState::new(launch.settings.clone(), registry);

    for state in poller::refresh_targets_once(launch.targets.clone(), launch.settings.clone()).await
    {
        app.apply_update(state);
    }

    let mut discovery_rx = discovery::start(
        launch.discovery_targets,
        launch.discovery_seed_targets,
        launch.targets,
        launch.settings,
    );
    while let Some(event) = discovery_rx.recv().await {
        app.apply_discovery_event(&event);
        if let DiscoveryEvent::VerificationSucceeded(verified) = &event {
            app.apply_verified_instance((**verified).clone());
        }
        if matches!(event, DiscoveryEvent::Complete) {
            break;
        }
    }

    let mut stdout = io::stdout().lock();
    let frame = app.build_overview_frame();
    match launch.output_mode {
        OutputMode::Tui => {
            let output = render_plain_text(&frame);
            stdout.write_all(output.as_bytes())?;
            if !output.ends_with('\n') {
                stdout.write_all(b"\n")?;
            }
        }
        OutputMode::Json => {
            serde_json::to_writer(&mut stdout, &frame)?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}

async fn run_json_stream(launch: LaunchConfig) -> Result<()> {
    let registry = ColumnRegistry::load(
        launch.config_path.as_deref(),
        launch.no_default_config,
        launch.settings.default_sort,
    );
    let mut app = AppState::new(launch.settings.clone(), registry);
    let (mut updates_rx, request_tx) =
        poller::start(launch.targets.clone(), launch.settings.clone());
    let mut discovery_rx = discovery::start(
        launch.discovery_targets,
        launch.discovery_seed_targets,
        launch.targets,
        launch.settings.clone(),
    );
    let mut frame_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        frame_interval.tick().await;
        drain_updates(&mut app, &mut updates_rx, &mut discovery_rx, &request_tx);
        let frame = app.build_overview_frame();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        serde_json::to_writer(&mut stdout, &frame)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}

#[allow(clippy::too_many_lines)]
fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, launch: LaunchConfig) -> Result<()> {
    let registry = ColumnRegistry::load(
        launch.config_path.as_deref(),
        launch.no_default_config,
        launch.settings.default_sort,
    );
    let mut app = AppState::new(launch.settings.clone(), registry);
    let (mut updates_rx, request_tx) =
        poller::start(launch.targets.clone(), launch.settings.clone());
    let mut discovery_rx = discovery::start(
        launch.discovery_targets,
        launch.discovery_seed_targets,
        launch.targets,
        launch.settings,
    );

    loop {
        drain_updates(&mut app, &mut updates_rx, &mut discovery_rx, &request_tx);

        maybe_request_bigkeys_scan(&mut app, &request_tx);

        terminal.draw(|frame| draw(frame, &mut app))?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if is_force_quit_key(key) {
                app.should_quit = true;
                continue;
            }

            if app.is_auth_form_open() {
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    match key.code {
                        KeyCode::Esc => app.close_auth_form(),
                        KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                            app.toggle_auth_field();
                        }
                        KeyCode::Enter => {
                            let is_username = app
                                .auth_form
                                .as_ref()
                                .is_some_and(|form| form.active_field == AuthField::Username);
                            if is_username {
                                app.toggle_auth_field();
                            } else if let Some((key, username, password)) =
                                app.take_auth_credentials()
                            {
                                let _ = request_tx.try_send(PollerRequest::AuthenticateTarget {
                                    key,
                                    username,
                                    password,
                                });
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some(value) = app.auth_active_value_mut() {
                                let _ = value.pop();
                            }
                        }
                        KeyCode::Char(ch) => {
                            if let Some(value) = app.auth_active_value_mut() {
                                value.push(ch);
                            }
                        }
                        _ => {}
                    }
                }
                continue;
            }

            if handle_overlay_quit_key(&mut app, key) {
                continue;
            }

            if app.is_filtering {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        app.is_filtering = false;
                        app.clamp_selection();
                    }
                    KeyCode::Backspace => {
                        let _ = app.filter.pop();
                        app.clamp_selection();
                    }
                    KeyCode::Char(ch) => {
                        app.filter.push(ch);
                        app.clamp_selection();
                    }
                    KeyCode::F(3) if app.active_view == ActiveView::Overview => {
                        app.start_filter_input(FilterPromptMode::Search, false);
                    }
                    KeyCode::F(4) if app.active_view == ActiveView::Overview => {
                        app.start_filter_input(FilterPromptMode::Filter, true);
                    }
                    _ => {}
                }
                continue;
            }

            if app.commandstats_view.is_filtering {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        app.commandstats_view.is_filtering = false;
                    }
                    KeyCode::Backspace => {
                        let _ = app.commandstats_view.filter.pop();
                        sync_commandstats_view(&mut app, terminal.size()?.height);
                    }
                    KeyCode::Char(ch) => {
                        app.commandstats_view.filter.push(ch);
                        sync_commandstats_view(&mut app, terminal.size()?.height);
                    }
                    _ => {}
                }
                continue;
            }

            if app.bigkeys_view.is_filtering {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        app.bigkeys_view.is_filtering = false;
                    }
                    KeyCode::Backspace => {
                        let _ = app.bigkeys_view.filter.pop();
                        sync_bigkeys_view(&mut app, terminal.size()?.height);
                    }
                    KeyCode::Char(ch) => {
                        app.bigkeys_view.filter.push(ch);
                        sync_bigkeys_view(&mut app, terminal.size()?.height);
                    }
                    _ => {}
                }
                continue;
            }

            if app.hotkeys_view.is_filtering {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        app.hotkeys_view.is_filtering = false;
                    }
                    KeyCode::Backspace => {
                        let _ = app.hotkeys_view.filter.pop();
                        sync_hotkeys_view(&mut app, terminal.size()?.height);
                    }
                    KeyCode::Char(ch) => {
                        app.hotkeys_view.filter.push(ch);
                        sync_hotkeys_view(&mut app, terminal.size()?.height);
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(view) = app.active_detail_view_mut()
                && view.is_filtering
            {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        view.is_filtering = false;
                    }
                    KeyCode::Backspace => {
                        let _ = view.filter.pop();
                        sync_detail_views(&mut app, terminal.size()?.height);
                    }
                    KeyCode::Char(ch) => {
                        view.filter.push(ch);
                        sync_detail_views(&mut app, terminal.size()?.height);
                    }
                    _ => {}
                }
                continue;
            }

            if app.is_sort_picker_open() {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
                    KeyCode::Up => app.move_sort_picker_selection(-1),
                    KeyCode::Down => app.move_sort_picker_selection(1),
                    KeyCode::Enter => app.apply_sort_picker_selection(),
                    _ => {}
                }
                continue;
            }

            if app.is_column_picker_open() && handle_column_picker_key(&mut app, key) {
                continue;
            }

            if app.is_kill_picker_open() {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
                    KeyCode::Up => app.move_kill_picker_selection(-1),
                    KeyCode::Down => app.move_kill_picker_selection(1),
                    KeyCode::Enter => {
                        if let (Some(key), Some(action)) =
                            (app.selected_key(), app.selected_kill_action())
                        {
                            let _ = request_tx.try_send(PollerRequest::KillTarget { key, action });
                        }
                        app.close_overview_modal();
                    }
                    _ => {}
                }
                continue;
            }

            if key.kind != KeyEventKind::Press {
                continue;
            }

            if handle_overview_shortcut(&mut app, key) {
                continue;
            }

            match key.code {
                KeyCode::F(1) | KeyCode::Char('H') => app.open_help_view(),
                KeyCode::Char('?') => app.show_help = !app.show_help,
                KeyCode::Char('/') if is_commandstats_detail(&app) => {
                    app.start_active_detail_filter_input(false);
                    sync_commandstats_view(&mut app, terminal.size()?.height);
                }
                KeyCode::Char('/') if is_bigkeys_detail(&app) => {
                    app.start_active_detail_filter_input(false);
                    sync_bigkeys_view(&mut app, terminal.size()?.height);
                }
                KeyCode::Char('/') if is_hotkeys_detail(&app) => {
                    app.start_active_detail_filter_input(false);
                    sync_hotkeys_view(&mut app, terminal.size()?.height);
                }
                KeyCode::Char('/') if is_detail_text_tab(&app) => {
                    app.start_active_detail_filter_input(false);
                    sync_detail_views(&mut app, terminal.size()?.height);
                }
                KeyCode::Char('c' | 'C') if is_hotkeys_detail(&app) => {
                    start_hotkeys_sampling(&mut app, &request_tx, HotkeysMetric::Cpu, true);
                }
                KeyCode::Char('n' | 'N') if is_hotkeys_detail(&app) => {
                    start_hotkeys_sampling(&mut app, &request_tx, HotkeysMetric::Net, true);
                }
                KeyCode::Char('x' | 'X') if is_hotkeys_detail(&app) => {
                    handle_hotkeys_stop_or_reset(&mut app, &request_tx);
                }
                KeyCode::Char('r' | 'R') => {
                    let request = if is_hotkeys_detail(&app) {
                        let metric = current_hotkeys(&app)
                            .and_then(|hotkeys| hotkeys.selected_metric)
                            .unwrap_or(HotkeysMetric::Cpu);
                        start_hotkeys_sampling(&mut app, &request_tx, metric, true);
                        None
                    } else if is_bigkeys_detail(&app) {
                        app.selected_key().map(|key| {
                            mark_bigkeys_running(&mut app, &key);
                            PollerRequest::RefreshBigkeys { key, force: true }
                        })
                    } else {
                        Some(PollerRequest::RefreshAll)
                    };
                    if let Some(request) = request {
                        let _ = request_tx.try_send(request);
                    }
                }
                KeyCode::Up if app.active_view == ActiveView::Overview => app.move_selection(-1),
                KeyCode::Down if app.active_view == ActiveView::Overview => app.move_selection(1),
                KeyCode::Up if is_commandstats_detail(&app) => {
                    if let Some(stats) = current_commandstats(&app).map(ToOwned::to_owned) {
                        let page_len = commandstats_page_len(terminal.size()?.height);
                        app.move_commandstats_scroll(-1, &stats, page_len);
                    }
                }
                KeyCode::Down if is_commandstats_detail(&app) => {
                    if let Some(stats) = current_commandstats(&app).map(ToOwned::to_owned) {
                        let page_len = commandstats_page_len(terminal.size()?.height);
                        app.move_commandstats_scroll(1, &stats, page_len);
                    }
                }
                KeyCode::Up if is_bigkeys_detail(&app) => {
                    if let Some(bigkeys) = current_bigkeys(&app) {
                        let page_len = bigkeys_page_len(terminal.size()?.height);
                        let visible_len = app.visible_bigkeys(&bigkeys.largest_keys).len();
                        app.move_bigkeys_scroll(-1, visible_len, page_len);
                    }
                }
                KeyCode::Down if is_bigkeys_detail(&app) => {
                    if let Some(bigkeys) = current_bigkeys(&app) {
                        let page_len = bigkeys_page_len(terminal.size()?.height);
                        let visible_len = app.visible_bigkeys(&bigkeys.largest_keys).len();
                        app.move_bigkeys_scroll(1, visible_len, page_len);
                    }
                }
                KeyCode::Up if is_hotkeys_detail(&app) => {
                    if let Some(hotkeys) = current_hotkeys(&app) {
                        let page_len = hotkeys_page_len(terminal.size()?.height);
                        let visible_len = app.visible_hotkeys(&hotkeys.entries).len();
                        app.move_hotkeys_scroll(-1, visible_len, page_len);
                    }
                }
                KeyCode::Down if is_hotkeys_detail(&app) => {
                    if let Some(hotkeys) = current_hotkeys(&app) {
                        let page_len = hotkeys_page_len(terminal.size()?.height);
                        let visible_len = app.visible_hotkeys(&hotkeys.entries).len();
                        app.move_hotkeys_scroll(1, visible_len, page_len);
                    }
                }
                KeyCode::Up if is_detail_text_tab(&app) => {
                    let page_len = detail_text_page_len(terminal.size()?.height);
                    let row_count = current_detail_text_body(&app).map_or(0, |body| {
                        let lines = detail_text_lines(&body);
                        app.visible_detail_text_lines(app.detail_tab, &lines).len()
                    });
                    app.move_detail_text_scroll(app.detail_tab, -1, row_count, page_len);
                }
                KeyCode::Down if is_detail_text_tab(&app) => {
                    let page_len = detail_text_page_len(terminal.size()?.height);
                    let row_count = current_detail_text_body(&app).map_or(0, |body| {
                        let lines = detail_text_lines(&body);
                        app.visible_detail_text_lines(app.detail_tab, &lines).len()
                    });
                    app.move_detail_text_scroll(app.detail_tab, 1, row_count, page_len);
                }
                KeyCode::Enter
                    if app.active_view == ActiveView::Overview && app.selected_key().is_some() =>
                {
                    app.active_view = ActiveView::Detail;
                    sync_detail_views(&mut app, terminal.size()?.height);
                }
                KeyCode::Char('q') | KeyCode::Esc
                    if handle_primary_view_quit_key(&mut app, key) => {}
                KeyCode::Esc if app.active_view == ActiveView::Detail => {
                    app.close_detail_view();
                }
                KeyCode::Esc if app.active_view == ActiveView::Help => app.close_help_view(),
                KeyCode::Tab | KeyCode::Right if app.active_view == ActiveView::Detail => {
                    app.detail_tab = (app.detail_tab + 1) % DETAIL_TABS.len();
                    sync_detail_views(&mut app, terminal.size()?.height);
                }
                KeyCode::Left if app.active_view == ActiveView::Detail => {
                    app.detail_tab = (app.detail_tab + DETAIL_TABS.len() - 1) % DETAIL_TABS.len();
                    sync_detail_views(&mut app, terminal.size()?.height);
                }
                KeyCode::Char(ch) if app.active_view == ActiveView::Detail => {
                    if let Some(index) = detail_tab_index_for_shortcut(ch) {
                        app.detail_tab = index;
                        sync_detail_views(&mut app, terminal.size()?.height);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Handle overview commands after text inputs and modal dialogs have consumed their keys.
fn handle_overview_shortcut(app: &mut AppState, key: KeyEvent) -> bool {
    if app.active_view != ActiveView::Overview
        || app.is_filtering
        || app.overview_modal != OverviewModal::None
        || app.show_help
        || key.kind != KeyEventKind::Press
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }

    match key.code {
        KeyCode::F(5) | KeyCode::Char('t' | 'T') => app.cycle_view_mode(),
        KeyCode::F(6) | KeyCode::Char('s' | 'S') => app.open_sort_picker(),
        KeyCode::F(7) | KeyCode::Char('c' | 'C' | 'v') => app.open_column_picker(),
        KeyCode::F(8) | KeyCode::Char('a' | 'A') => app.open_auth_form(),
        KeyCode::F(9) | KeyCode::Char('k' | 'K') => {
            if app.selected_key().is_some() {
                app.open_kill_picker();
            }
        }
        KeyCode::Char('h') => {
            app.toggle_host_rendering();
            app.clamp_selection();
        }
        KeyCode::F(3) => app.start_filter_input(FilterPromptMode::Search, false),
        KeyCode::F(4) => app.start_filter_input(FilterPromptMode::Filter, true),
        KeyCode::Char('f' | 'F' | '/') => {
            app.start_filter_input(FilterPromptMode::Filter, false);
        }
        _ => return false,
    }
    true
}

fn drain_updates(
    app: &mut AppState,
    updates_rx: &mut tokio::sync::mpsc::Receiver<PollerUpdate>,
    discovery_rx: &mut tokio::sync::mpsc::Receiver<DiscoveryEvent>,
    request_tx: &tokio::sync::mpsc::Sender<PollerRequest>,
) {
    while let Ok(update) = updates_rx.try_recv() {
        match update {
            PollerUpdate::State(state) => app.apply_update(*state),
            PollerUpdate::Remove { key } => app.remove_instance(&key),
        }
    }
    while let Ok(event) = discovery_rx.try_recv() {
        app.apply_discovery_event(&event);
        if let DiscoveryEvent::VerificationSucceeded(verified) = event {
            let target = verified.target.clone();
            app.apply_verified_instance(*verified);
            let _ = request_tx.try_send(PollerRequest::UpsertTarget(target));
        }
    }
}

fn current_commandstats(app: &AppState) -> Option<&[crate::model::CommandStat]> {
    let key = app.selected_key()?;
    let instance = app.instances.get(&key)?;
    Some(&instance.detail.commandstats)
}

fn current_bigkeys(app: &AppState) -> Option<&crate::model::BigkeysMetrics> {
    let key = app.selected_key()?;
    let instance = app.instances.get(&key)?;
    Some(&instance.detail.bigkeys)
}

fn current_hotkeys(app: &AppState) -> Option<&crate::hotkeys::HotkeysMetrics> {
    let key = app.selected_key()?;
    let instance = app.instances.get(&key)?;
    Some(&instance.detail.hotkeys)
}

fn current_detail_text_body(app: &AppState) -> Option<String> {
    let key = app.selected_key()?;
    let instance = app.instances.get(&key)?;
    Some(detail_text_body(instance, app.detail_tab))
}

fn is_commandstats_detail(app: &AppState) -> bool {
    app.active_view == ActiveView::Detail && app.detail_tab == 3
}

fn is_bigkeys_detail(app: &AppState) -> bool {
    app.active_view == ActiveView::Detail && app.detail_tab == 4
}

fn is_hotkeys_detail(app: &AppState) -> bool {
    app.active_view == ActiveView::Detail && app.detail_tab == 5
}

fn is_detail_text_tab(app: &AppState) -> bool {
    app.active_view == ActiveView::Detail && app.detail_tab <= 2
}

fn sync_detail_text_view(app: &mut AppState, terminal_height: u16) {
    let detail_tab = app.detail_tab;
    let is_active_text_tab = app.active_view == ActiveView::Detail && detail_tab <= 2;
    if !is_active_text_tab {
        if let Some(view) = app.detail_text_view_mut(detail_tab) {
            view.is_filtering = false;
        }
        return;
    }

    let page_len = detail_text_page_len(terminal_height);
    let row_count = current_detail_text_body(app).map_or(0, |body| {
        let lines = detail_text_lines(&body);
        app.visible_detail_text_lines(detail_tab, &lines).len()
    });
    app.clamp_detail_text_scroll(detail_tab, row_count, page_len);
}

fn sync_commandstats_view(app: &mut AppState, terminal_height: u16) {
    if !is_commandstats_detail(app) {
        app.commandstats_view.is_filtering = false;
        return;
    }

    let page_len = commandstats_page_len(terminal_height);
    let stats =
        current_commandstats(app).map_or_else(Vec::new, <[crate::model::CommandStat]>::to_vec);
    app.clamp_commandstats_scroll(&stats, page_len);
}

fn sync_bigkeys_view(app: &mut AppState, terminal_height: u16) {
    if !is_bigkeys_detail(app) {
        app.bigkeys_view.is_filtering = false;
        return;
    }

    let page_len = bigkeys_page_len(terminal_height);
    let row_count = current_bigkeys(app).map_or(0, |bigkeys| {
        app.visible_bigkeys(&bigkeys.largest_keys).len()
    });
    app.clamp_bigkeys_scroll(row_count, page_len);
}

fn sync_hotkeys_view(app: &mut AppState, terminal_height: u16) {
    if !is_hotkeys_detail(app) {
        app.hotkeys_view.is_filtering = false;
        return;
    }

    let page_len = hotkeys_page_len(terminal_height);
    let row_count =
        current_hotkeys(app).map_or(0, |hotkeys| app.visible_hotkeys(&hotkeys.entries).len());
    app.clamp_hotkeys_scroll(row_count, page_len);
}

fn sync_detail_views(app: &mut AppState, terminal_height: u16) {
    sync_detail_text_view(app, terminal_height);
    sync_commandstats_view(app, terminal_height);
    sync_bigkeys_view(app, terminal_height);
    sync_hotkeys_view(app, terminal_height);
}

const fn commandstats_page_len(area_height: u16) -> usize {
    if area_height <= 6 {
        1
    } else {
        area_height as usize - 6
    }
}

const fn bigkeys_page_len(area_height: u16) -> usize {
    if area_height <= 5 {
        1
    } else {
        area_height as usize - 5
    }
}

const fn hotkeys_page_len(area_height: u16) -> usize {
    if area_height <= 5 {
        1
    } else {
        area_height as usize - 5
    }
}

const fn detail_text_page_len(area_height: u16) -> usize {
    if area_height <= 2 {
        1
    } else {
        area_height as usize - 2
    }
}

fn maybe_request_bigkeys_scan(
    app: &mut AppState,
    request_tx: &tokio::sync::mpsc::Sender<PollerRequest>,
) {
    if !is_bigkeys_detail(app) {
        return;
    }

    let Some(key) = app.selected_key() else {
        return;
    };
    let should_request = app.instances.get(&key).is_some_and(|instance| {
        matches!(
            instance.detail.bigkeys.status,
            crate::model::BigkeysScanStatus::Idle
        )
    });
    if should_request {
        mark_bigkeys_running(app, &key);
        let _ = request_tx.try_send(PollerRequest::RefreshBigkeys { key, force: false });
    }
}

fn mark_bigkeys_running(app: &mut AppState, key: &str) {
    if let Some(instance) = app.instances.get_mut(key) {
        instance.detail.bigkeys.status = crate::model::BigkeysScanStatus::Running;
        instance.detail.bigkeys.last_error = None;
    }
}

fn start_hotkeys_sampling(
    app: &mut AppState,
    request_tx: &tokio::sync::mpsc::Sender<PollerRequest>,
    metric: HotkeysMetric,
    force: bool,
) {
    let Some(key) = app.selected_key() else {
        return;
    };

    app.clear_hotkeys_local_reset(&key);
    if let Some(instance) = app.instances.get_mut(&key) {
        instance
            .detail
            .hotkeys
            .start(metric, std::time::Duration::from_mins(1));
    }
    let _ = request_tx.try_send(PollerRequest::StartHotkeys { key, metric, force });
}

fn handle_hotkeys_stop_or_reset(
    app: &mut AppState,
    request_tx: &tokio::sync::mpsc::Sender<PollerRequest>,
) {
    let Some(key) = app.selected_key() else {
        return;
    };

    let is_running = app
        .instances
        .get(&key)
        .is_some_and(|instance| instance.detail.hotkeys.status == HotkeysStatus::Running);

    if is_running {
        let _ = request_tx.try_send(PollerRequest::StopHotkeys { key });
        return;
    }

    app.reset_hotkeys_locally(&key);
    sync_hotkeys_view(app, 0);
}

const fn is_force_quit_key(key: KeyEvent) -> bool {
    matches!(
        key,
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        } if modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn handle_overlay_quit_key(app: &mut AppState, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return false;
    }

    if !matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
        return false;
    }

    if app.show_help {
        app.show_help = false;
        return true;
    }

    if app.is_sort_picker_open() || app.is_column_picker_open() || app.is_kill_picker_open() {
        app.close_overview_modal();
        return true;
    }

    false
}

fn handle_primary_view_quit_key(app: &mut AppState, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return false;
    }

    if !matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
        return false;
    }

    if app.active_view != ActiveView::Overview
        || app.show_help
        || app.overview_modal != OverviewModal::None
    {
        return false;
    }

    app.should_quit = true;
    true
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut AppState) {
    let area = frame.area();
    frame.render_widget(Block::default().style(base_style(app)), area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(area);

    match app.active_view {
        ActiveView::Overview => draw_overview(frame, app, chunks[0]),
        ActiveView::Detail => draw_detail(frame, app, chunks[0]),
        ActiveView::Help => draw_help_page(frame, app, chunks[0]),
    }
    draw_status_bar(frame, app, chunks[1]);

    if app.is_sort_picker_open() {
        draw_sort_picker(frame, area, app);
    }

    if app.is_column_picker_open() {
        draw_column_picker(frame, area, app);
    }

    if app.is_kill_picker_open() {
        draw_kill_picker(frame, area, app);
    }

    if app.is_auth_form_open() {
        draw_auth_form(frame, area, app);
    }

    if app.show_help {
        draw_help_overlay(frame, app, area);
    }
}

fn overview_cell(fitted: String, emphasis_style: Option<EmphasisStyle>) -> Cell<'static> {
    let Some(emphasis_style) = emphasis_style else {
        return Cell::from(fitted);
    };

    Cell::from(Line::styled(fitted, style_from_emphasis(emphasis_style)))
}

fn overview_cluster_gutter_cell(
    cluster_gutter: Option<&crate::overview::OverviewClusterGutter>,
) -> Cell<'static> {
    let Some(cluster_gutter) = cluster_gutter else {
        return Cell::from(" ");
    };

    Cell::from(Line::from(vec![Span::styled(
        "│",
        Style::default().fg(ratatui_color_from_cluster(cluster_gutter.color)),
    )]))
}

const fn ratatui_color_from_cluster(color: ClusterGutterColor) -> Color {
    match color {
        ClusterGutterColor::Cyan => Color::Cyan,
        ClusterGutterColor::Yellow => Color::Yellow,
        ClusterGutterColor::Green => Color::Green,
        ClusterGutterColor::Magenta => Color::Magenta,
        ClusterGutterColor::Blue => Color::Blue,
        ClusterGutterColor::Red => Color::Red,
        ClusterGutterColor::Gray => Color::Gray,
    }
}

fn style_from_emphasis(emphasis_style: EmphasisStyle) -> Style {
    let mut style = Style::default();
    if emphasis_style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if emphasis_style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if emphasis_style.underlined {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if emphasis_style.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if emphasis_style.reversed {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if let Some(color) = emphasis_style.foreground {
        style = style.fg(color.to_ratatui_color());
    }
    style
}

#[allow(clippy::too_many_lines)]
fn draw_overview(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    const TABLE_COLUMN_SPACING: u16 = 1;
    let overview = app.build_overview_frame();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(5)])
        .split(area);

    let header = Paragraph::new(format!(
        "redis-top  refresh={}  view={:?}  sort={} {}  host={}  filter={}{}",
        humantime::format_duration(app.settings.refresh_interval),
        app.view_mode,
        overview.header.sort.label,
        sort_direction_symbol(app.sort_direction),
        match overview.header.host_rendering {
            "shown" => "shown",
            "omitted_auto" => "omitted(auto)",
            _ => "shown(auto)",
        },
        if overview.header.filter.is_empty() {
            "<none>"
        } else {
            &overview.header.filter
        },
        if overview.header.is_filtering {
            " (editing)"
        } else {
            ""
        }
    ))
    .style(base_style(app))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Overview")
            .style(base_style(app)),
    );
    frame.render_widget(header, chunks[0]);

    let column_keys = overview
        .columns
        .iter()
        .map(|column| column.key.clone())
        .collect::<Vec<_>>();
    let columns: Vec<_> = column_keys
        .iter()
        .filter_map(|key| app.column_registry.column(key.as_str()))
        .collect();
    let widths = compute_column_widths(
        chunks[1].width.saturating_sub(2),
        &columns,
        TABLE_COLUMN_SPACING,
    );

    let table_rows: Vec<Row<'_>> = overview
        .rows
        .iter()
        .map(|row| {
            let mut cells = Vec::with_capacity(overview.columns.len() + 1);
            cells.push(overview_cluster_gutter_cell(row.cluster_gutter.as_ref()));
            cells.extend(row.cells.iter().enumerate().map(|(idx, cell)| {
                let width = widths[idx];
                let align = columns[idx].align();
                let fitted = fit_cell_text(&cell.value, width as usize, align);
                let emphasis_style = cell.emphasized.then(|| {
                    columns[idx]
                        .emphasis_style()
                        .unwrap_or_else(|| app.column_registry.overview_emphasis_style())
                });
                overview_cell(fitted, emphasis_style)
            }));

            let base = Row::new(cells);
            if row.stale {
                base.style(base_style(app).add_modifier(Modifier::DIM))
            } else {
                base.style(base_style(app))
            }
        })
        .collect();

    let constraints: Vec<Constraint> = std::iter::once(Constraint::Length(1))
        .chain(widths.iter().copied().map(Constraint::Length))
        .collect();
    let header = Row::new(
        std::iter::once(Cell::from(" ")).chain(
            columns
                .iter()
                .zip(column_keys.iter())
                .zip(widths.iter())
                .map(|((column, key), width)| {
                    let label =
                        sortable_header(column.header(), &app.sort_by, app.sort_direction, key);
                    Cell::from(fit_cell_text(&label, *width as usize, column.align()))
                }),
        ),
    )
    .style(base_style(app).add_modifier(Modifier::BOLD));

    let selected_style = Style::default().bg(background_color(app));
    let table = Table::new(table_rows, constraints)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(base_style(app)),
        )
        .column_spacing(TABLE_COLUMN_SPACING)
        .row_highlight_style(selected_style)
        .highlight_symbol("> ");

    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.selected_index));
    frame.render_stateful_widget(table, chunks[1], &mut state);
}

fn compute_column_widths(
    table_width: u16,
    columns: &[&std::sync::Arc<dyn crate::column::Column>],
    column_spacing: u16,
) -> Vec<u16> {
    if columns.is_empty() {
        return Vec::new();
    }

    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let spacing_total = column_spacing.saturating_mul(gaps);
    let content_width = table_width.saturating_sub(spacing_total);
    let mut widths = vec![0u16; columns.len()];
    let mut remaining = content_width;

    for (idx, column) in columns.iter().enumerate() {
        let hint = column.width_hint();
        if let Some(fixed) = hint.fixed {
            widths[idx] = fixed;
            remaining = remaining.saturating_sub(fixed);
        }
    }

    for (idx, column) in columns.iter().enumerate() {
        if widths[idx] > 0 {
            continue;
        }
        let min = column.width_hint().min;
        widths[idx] = min;
        remaining = remaining.saturating_sub(min);
    }

    let used = widths.iter().copied().sum::<u16>();
    if used > content_width {
        shrink_widths_to_fit(&mut widths, content_width);
        remaining = 0;
    }

    loop {
        if remaining == 0 {
            break;
        }
        let mut progressed = false;
        for (idx, column) in columns.iter().enumerate() {
            let hint = column.width_hint();
            if hint.fixed.is_some() {
                continue;
            }
            let max = hint.max.unwrap_or(u16::MAX);
            let ideal = hint.ideal.min(max);
            if widths[idx] < ideal {
                widths[idx] = widths[idx].saturating_add(1);
                remaining = remaining.saturating_sub(1);
                progressed = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    widths
}

fn shrink_widths_to_fit(widths: &mut [u16], target: u16) {
    while widths.iter().copied().sum::<u16>() > target {
        let Some((idx, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width) else {
            break;
        };
        if widths[idx] <= 1 {
            break;
        }
        widths[idx] -= 1;
    }
}

#[allow(clippy::too_many_lines)]
fn draw_detail(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let Some(selected_key) = app.selected_key() else {
        frame.render_widget(
            Paragraph::new("No instance selected")
                .style(base_style(app))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Detail")
                        .style(base_style(app)),
                ),
            area,
        );
        return;
    };
    let Some(instance) = app.instances.get(&selected_key) else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    let title = format!(
        "{} ({})  role={}  status={}  version={} uptime={}s",
        instance
            .alias
            .clone()
            .unwrap_or_else(|| instance.addr.clone()),
        instance.addr,
        instance.kind.as_str(),
        instance.status.as_str(),
        instance
            .detail
            .redis_version
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        instance
            .detail
            .uptime_seconds
            .map_or_else(|| "-".to_string(), format_with_commas)
    );
    frame.render_widget(
        Paragraph::new(title).style(base_style(app)).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Instance")
                .style(base_style(app)),
        ),
        chunks[0],
    );

    match app.detail_tab {
        0 => {
            draw_detail_text(
                frame,
                app,
                chunks[1],
                0,
                "Summary",
                &detail_text_body(instance, 0),
            );
        }
        1 => {
            draw_detail_text(
                frame,
                app,
                chunks[1],
                1,
                "Latency",
                &detail_text_body(instance, 1),
            );
        }
        2 => draw_detail_text(
            frame,
            app,
            chunks[1],
            2,
            "Info Raw",
            &detail_text_body(instance, 2),
        ),
        3 => draw_commandstats(frame, app, chunks[1], &instance.detail.commandstats),
        4 => draw_bigkeys(frame, app, chunks[1], &instance.detail.bigkeys),
        _ => draw_hotkeys(frame, app, chunks[1], &instance.detail.hotkeys),
    }
}

fn detail_tab_index_for_shortcut(ch: char) -> Option<usize> {
    let shortcut = ch.to_ascii_lowercase();
    DETAIL_TABS.iter().position(|tab| tab.shortcut == shortcut)
}

#[cfg(test)]
fn detail_tabs_widget(app: &AppState) -> ratatui::widgets::Tabs<'static> {
    ratatui::widgets::Tabs::new(DETAIL_TABS.iter().map(detail_tab_label))
        .style(base_style(app))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(base_style(app)),
        )
        .divider("│")
        .select(app.detail_tab)
        .highlight_style(
            Style::default()
                .fg(background_color(app))
                .bg(carat_color(app))
                .add_modifier(Modifier::BOLD),
        )
}

fn draw_commandstats(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    stats: &[crate::model::CommandStat],
) {
    if stats.is_empty() {
        frame.render_widget(
            Paragraph::new("INFO COMMANDSTATS not available")
                .style(base_style(app))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Commandstats")
                        .style(base_style(app)),
                ),
            area,
        );
        return;
    }

    let visible_stats = app.visible_commandstats(stats);
    if visible_stats.is_empty() {
        frame.render_widget(
            Paragraph::new("No commandstats match the current filter")
                .style(base_style(app))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(commandstats_title(app, 0, 0, 0))
                        .style(base_style(app)),
                ),
            area,
        );
        return;
    }

    let page_len = commandstats_page_len(area.height);
    let start = app
        .commandstats_view
        .scroll_offset
        .min(visible_stats.len().saturating_sub(page_len.max(1)));
    let end = (start + page_len).min(visible_stats.len());
    let rows: Vec<Row<'_>> = visible_stats[start..end]
        .iter()
        .map(|stat| {
            Row::new(vec![
                Cell::from(stat.command.clone()),
                Cell::from(Line::from(format_with_commas(stat.calls)).right_aligned()),
                Cell::from(Line::from(format_with_commas(stat.usec)).right_aligned()),
                Cell::from(Line::from(format!("{:.2}", stat.usec_per_call)).right_aligned()),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(15),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Command"),
            Cell::from(Line::from("Calls").right_aligned()),
            Cell::from(Line::from("Usec").right_aligned()),
            Cell::from(Line::from("Usec/Call").right_aligned()),
        ])
        .style(base_style(app).add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(commandstats_title(app, start, end, visible_stats.len()))
            .style(base_style(app)),
    )
    .style(base_style(app))
    .column_spacing(1);

    frame.render_widget(table, area);
}

fn draw_bigkeys(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    bigkeys: &crate::model::BigkeysMetrics,
) {
    let visible_total = app.visible_bigkeys(&bigkeys.largest_keys).len();
    let block = bigkeys_block(app, bigkeys, visible_total, 0, visible_total);

    if matches!(bigkeys.status, crate::model::BigkeysScanStatus::Running)
        && bigkeys.largest_keys.is_empty()
    {
        frame.render_widget(
            Paragraph::new("Scanning keyspace for big keys...")
                .style(base_style(app))
                .block(block),
            area,
        );
        return;
    }

    if let Some(error) = &bigkeys.last_error
        && bigkeys.largest_keys.is_empty()
    {
        frame.render_widget(
            Paragraph::new(error.clone())
                .style(base_style(app))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if bigkeys.largest_keys.is_empty() {
        frame.render_widget(
            Paragraph::new("No keys found")
                .style(base_style(app))
                .block(block),
            area,
        );
        return;
    }

    if visible_total == 0 {
        frame.render_widget(
            Paragraph::new("No keys match the current filter")
                .style(base_style(app))
                .block(block),
            area,
        );
        return;
    }

    frame.render_widget(bigkeys_table(app, area.height, bigkeys), area);
}

fn draw_hotkeys(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    hotkeys: &crate::hotkeys::HotkeysMetrics,
) {
    let visible_total = app.visible_hotkeys(&hotkeys.entries).len();
    let block = hotkeys_block(app, hotkeys, visible_total, 0, visible_total);

    if hotkeys.status == HotkeysStatus::Idle {
        frame.render_widget(
            Paragraph::new("Start sampling (60 seconds)\n\n[C] CPU [N] NET")
                .style(base_style(app))
                .block(block)
                .alignment(ratatui::layout::Alignment::Center)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if hotkeys.status == HotkeysStatus::Running {
        let remaining = hotkeys.remaining_seconds().unwrap_or(0);
        frame.render_widget(
            Paragraph::new(format!("\n\nSampling {remaining}s\nPress [X] to stop"))
                .style(base_style(app))
                .block(block)
                .alignment(ratatui::layout::Alignment::Center),
            area,
        );
        return;
    }

    if let Some(error) = &hotkeys.last_error
        && hotkeys.entries.is_empty()
    {
        frame.render_widget(
            Paragraph::new(error.clone())
                .style(base_style(app))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if hotkeys.entries.is_empty() {
        frame.render_widget(
            Paragraph::new("No hotkeys found\n\nPress [C] or [N] to run again, [R] to rerun the last metric, or [X] to reset this pane.")
                .style(base_style(app))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if visible_total == 0 {
        frame.render_widget(
            Paragraph::new("No hotkeys match the current filter")
                .style(base_style(app))
                .block(block),
            area,
        );
        return;
    }

    frame.render_widget(hotkeys_table(app, area.height, hotkeys), area);
}

fn commandstats_title(app: &AppState, start: usize, end: usize, total: usize) -> String {
    let mut title = if total == 0 {
        "Commandstats".to_string()
    } else {
        format!("Commandstats {}-{} / {}", start + 1, end, total)
    };

    if !app.commandstats_view.filter.is_empty() {
        let _ = write!(title, "  filter=/{}", app.commandstats_view.filter);
    }

    title
}

fn bigkeys_title(
    app: &AppState,
    bigkeys: &crate::model::BigkeysMetrics,
    visible_total: usize,
    start: usize,
    end: usize,
) -> String {
    let mut title = if visible_total == 0 {
        "Bigkeys".to_string()
    } else {
        format!("Bigkeys {}-{} / {}", start + 1, end, visible_total)
    };
    if !app.bigkeys_view.filter.is_empty() {
        let _ = write!(title, "  filter=/{}", app.bigkeys_view.filter);
    }
    if matches!(bigkeys.status, crate::model::BigkeysScanStatus::Running) {
        let _ = write!(title, "  scanning");
    }
    if let Some(error) = &bigkeys.last_error {
        let _ = write!(title, "  error={}", truncate_for_title(error, 40));
    }
    title
}

fn hotkeys_title(
    app: &AppState,
    hotkeys: &crate::hotkeys::HotkeysMetrics,
    visible_total: usize,
    start: usize,
    end: usize,
) -> String {
    let metric = hotkeys.selected_metric.map_or("?", HotkeysMetric::label);
    let mut title = if visible_total == 0 {
        format!("Hotkeys {metric}")
    } else {
        format!("Hotkeys {metric} {}-{} / {}", start + 1, end, visible_total)
    };
    if !app.hotkeys_view.filter.is_empty() {
        let _ = write!(title, "  filter=/{}", app.hotkeys_view.filter);
    }
    if hotkeys.status != HotkeysStatus::Idle && hotkeys.status != HotkeysStatus::Running {
        let _ = write!(title, "  C/N run  R rerun  X reset");
    }
    if let Some(error) = &hotkeys.last_error {
        let _ = write!(title, "  error={}", truncate_for_title(error, 40));
    }
    title
}

fn bigkeys_age_title(bigkeys: &crate::model::BigkeysMetrics) -> Option<Line<'static>> {
    if matches!(bigkeys.status, crate::model::BigkeysScanStatus::Running) {
        return None;
    }

    bigkeys
        .last_completed
        .map(|instant| Line::from(format!("age: {}s", instant.elapsed().as_secs())).right_aligned())
}

fn bigkeys_block(
    app: &AppState,
    bigkeys: &crate::model::BigkeysMetrics,
    visible_total: usize,
    start: usize,
    end: usize,
) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(bigkeys_title(app, bigkeys, visible_total, start, end))
        .style(base_style(app));

    if let Some(age) = bigkeys_age_title(bigkeys) {
        block = block.title(age);
    }

    block
}

fn hotkeys_block(
    app: &AppState,
    hotkeys: &crate::hotkeys::HotkeysMetrics,
    visible_total: usize,
    start: usize,
    end: usize,
) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(hotkeys_title(app, hotkeys, visible_total, start, end))
        .style(base_style(app));

    if let Some(instant) = hotkeys.last_completed {
        block = block
            .title(Line::from(format!("age: {}s", instant.elapsed().as_secs())).right_aligned());
    }

    block
}

fn bigkeys_table<'a>(
    app: &'a AppState,
    area_height: u16,
    bigkeys: &'a crate::model::BigkeysMetrics,
) -> Table<'a> {
    let visible = app.visible_bigkeys(&bigkeys.largest_keys);
    let page_len = bigkeys_page_len(area_height);
    let start = app
        .bigkeys_view
        .scroll_offset
        .min(visible.len().saturating_sub(page_len.max(1)));
    let end = (start + page_len).min(visible.len());
    let rows: Vec<Row<'a>> = visible[start..end]
        .iter()
        .map(|entry| {
            Row::new(vec![
                Cell::from(entry.key.clone()),
                Cell::from(entry.key_type.clone()),
                Cell::from(Line::from(format_optional_u64(entry.size)).right_aligned()),
                Cell::from(Line::from(format_optional_bytes(entry.memory_usage)).right_aligned()),
            ])
        })
        .collect();

    Table::new(
        rows,
        [
            Constraint::Min(26),
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(18),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Key"),
            Cell::from("Type"),
            Cell::from(Line::from("Length").right_aligned()),
            Cell::from(Line::from("Memory").right_aligned()),
        ])
        .style(base_style(app).add_modifier(Modifier::BOLD)),
    )
    .block(bigkeys_block(app, bigkeys, visible.len(), start, end))
    .style(base_style(app))
    .column_spacing(1)
}

fn hotkeys_table<'a>(
    app: &'a AppState,
    area_height: u16,
    hotkeys: &'a crate::hotkeys::HotkeysMetrics,
) -> Table<'a> {
    let visible = app.visible_hotkeys(&hotkeys.entries);
    let page_len = hotkeys_page_len(area_height);
    let start = app
        .hotkeys_view
        .scroll_offset
        .min(visible.len().saturating_sub(page_len.max(1)));
    let end = (start + page_len).min(visible.len());
    let total_value = hotkeys.total_value.unwrap_or(0);
    let value_header = hotkeys
        .selected_metric
        .map_or("Value", HotkeysMetric::value_header);
    let rows: Vec<Row<'a>> = visible[start..end]
        .iter()
        .map(|entry| {
            let share = if total_value == 0 {
                0.0
            } else {
                crate::column::u64_to_f64(entry.value) / crate::column::u64_to_f64(total_value)
                    * 100.0
            };
            Row::new(vec![
                Cell::from(entry.key.clone()),
                Cell::from(Line::from(format_with_commas(entry.value)).right_aligned()),
                Cell::from(Line::from(format!("{share:.2}%")).right_aligned()),
            ])
        })
        .collect();

    Table::new(
        rows,
        [
            Constraint::Min(26),
            Constraint::Length(18),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Key"),
            Cell::from(Line::from(value_header).right_aligned()),
            Cell::from(Line::from("Share").right_aligned()),
        ])
        .style(base_style(app).add_modifier(Modifier::BOLD)),
    )
    .block(hotkeys_block(app, hotkeys, visible.len(), start, end))
    .style(base_style(app))
    .column_spacing(1)
}

fn detail_tab_label(tab: &DetailTabSpec) -> Line<'static> {
    let shortcut = tab.shortcut.to_ascii_uppercase();
    let title = tab.title;

    if let Some((start, ch)) = title
        .char_indices()
        .find(|(_, ch)| ch.to_ascii_uppercase() == shortcut)
    {
        let end = start + ch.len_utf8();
        let prefix = &title[..start];
        let suffix = &title[end..];

        return Line::from(vec![
            Span::raw(prefix.to_string()),
            Span::raw("["),
            Span::styled(
                ch.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("]{suffix}")),
        ]);
    }

    Line::from(vec![Span::raw(title.to_string())])
}

fn format_aligned_rows(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(label, value)| format!("{label:width$} : {value}"))
        .collect::<Vec<String>>()
        .join("\n")
}

fn detail_text_body(instance: &crate::model::InstanceState, detail_tab: usize) -> String {
    match detail_tab {
        0 => summary_detail_body(instance),
        1 => latency_detail_body(instance),
        2 => instance
            .detail
            .raw_info
            .clone()
            .unwrap_or_else(|| "INFO not available".to_string()),
        _ => String::new(),
    }
}

fn summary_detail_body(instance: &crate::model::InstanceState) -> String {
    let hits = instance.detail.keyspace_hits.unwrap_or(0);
    let misses = instance.detail.keyspace_misses.unwrap_or(0);
    let hit_rate = if hits + misses == 0 {
        0.0
    } else {
        crate::column::u64_to_f64(hits) / crate::column::u64_to_f64(hits + misses) * 100.0
    };
    let replication_source = match (
        instance.detail.master_host.as_deref(),
        instance.detail.master_port,
    ) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => "-".to_string(),
    };
    let mut body = format_aligned_rows(&[
        ("status", instance.status.as_str().to_string()),
        (
            "used_memory",
            format_optional_bytes(instance.used_memory_bytes),
        ),
        (
            "used_memory_rss",
            format_optional_bytes(instance.detail.used_memory_rss),
        ),
        ("maxmemory", format_optional_bytes(instance.maxmemory_bytes)),
        ("ops_per_sec", format_optional_u64(instance.ops_per_sec)),
        (
            "commands",
            format_optional_u64(instance.detail.total_commands_processed),
        ),
        (
            "connected_clients",
            format_optional_u64(instance.detail.connected_clients),
        ),
        (
            "blocked_clients",
            format_optional_u64(instance.detail.blocked_clients),
        ),
        ("hits", format_with_commas(hits)),
        ("misses", format_with_commas(misses)),
        ("hit_rate", format!("{hit_rate:.1}%")),
        (
            "evicted_keys",
            format_optional_u64(instance.detail.evicted_keys),
        ),
        (
            "expired_keys",
            format_optional_u64(instance.detail.expired_keys),
        ),
        ("master", replication_source),
    ]);
    if let Some(details) = &instance.error_details {
        let _ = write!(
            body,
            "\n\nerror_summary : {}\nerror_details : {}",
            details.summary, details.message
        );
    }
    body
}

fn latency_detail_body(instance: &crate::model::InstanceState) -> String {
    format_aligned_rows(&[
        (
            "last_latency_ms",
            instance
                .last_latency_ms
                .map_or_else(|| "-".to_string(), |v| format!("{v:.2}")),
        ),
        ("max_latency_ms", format!("{:.2}", instance.max_latency_ms)),
        ("avg_latency_ms", format!("{:.2}", instance.avg_latency_ms)),
        (
            "window_samples",
            format_with_commas(instance.latency_window.len() as u64),
        ),
    ])
}

fn detail_text_lines(body: &str) -> Vec<String> {
    body.lines().map(ToOwned::to_owned).collect()
}

fn detail_text_title(
    app: &AppState,
    detail_tab: usize,
    base_title: &str,
    start: usize,
    end: usize,
    total: usize,
) -> String {
    let mut title = if total == 0 {
        base_title.to_string()
    } else {
        format!("{base_title} {}-{} / {}", start + 1, end, total)
    };
    if let Some(view) = app.detail_text_view(detail_tab)
        && !view.filter.is_empty()
    {
        let _ = write!(title, "  filter=/{}", view.filter);
    }
    title
}

fn draw_detail_text(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    detail_tab: usize,
    title: &str,
    body: &str,
) {
    let lines = detail_text_lines(body);
    let visible_lines = app.visible_detail_text_lines(detail_tab, &lines);
    let page_len = detail_text_page_len(area.height);
    let scroll_offset = app
        .detail_text_view(detail_tab)
        .map_or(0, |view| view.scroll_offset)
        .min(visible_lines.len().saturating_sub(page_len.max(1)));
    let end = (scroll_offset + page_len).min(visible_lines.len());
    let title = detail_text_title(
        app,
        detail_tab,
        title,
        scroll_offset,
        end,
        visible_lines.len(),
    );

    let body = if visible_lines.is_empty() {
        "No lines match the current filter".to_string()
    } else {
        visible_lines[scroll_offset..end].join("\n")
    };

    frame.render_widget(
        Paragraph::new(body)
            .style(base_style(app))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(base_style(app)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), format_with_commas)
}

fn format_optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), human_bytes)
}

fn truncate_for_title(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx == max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn format_with_commas(value: u64) -> String {
    let digits = value.to_string();
    let rev = digits.chars().rev().collect::<Vec<char>>();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in rev.iter().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(*ch);
    }
    out.chars().rev().collect()
}

fn human_bytes(bytes: u64) -> String {
    crate::column::format_bytes(bytes)
}

fn draw_help_page(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let rows: Vec<Row<'_>> = help_bindings()
        .iter()
        .map(|(keys, action)| Row::new(vec![Cell::from(*keys), Cell::from(*action)]))
        .collect();
    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(20)])
        .header(
            Row::new(vec!["Keys", "Action"]).style(base_style(app).add_modifier(Modifier::BOLD)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Help (Esc to go back)")
                .style(base_style(app)),
        );
    frame.render_widget(table, area);
}

fn draw_status_bar(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let prompt = if app.is_filtering {
        format!("{}: {}", app.filter_prompt_mode.label(), app.filter)
    } else if app.summary_view.is_filtering {
        format!("Summary Filter: /{}", app.summary_view.filter)
    } else if app.latency_view.is_filtering {
        format!("Latency Filter: /{}", app.latency_view.filter)
    } else if app.info_raw_view.is_filtering {
        format!("Info Raw Filter: /{}", app.info_raw_view.filter)
    } else if app.commandstats_view.is_filtering {
        format!("Commandstats Filter: /{}", app.commandstats_view.filter)
    } else if app.bigkeys_view.is_filtering {
        format!("Bigkeys Filter: /{}", app.bigkeys_view.filter)
    } else if app.hotkeys_view.is_filtering {
        format!("Hotkeys Filter: /{}", app.hotkeys_view.filter)
    } else if let Some(key) = app.selected_key() {
        app.instances
            .get(&key)
            .and_then(|instance| {
                instance
                    .error_details
                    .as_ref()
                    .map(|details| details.summary.clone())
                    .or_else(|| instance.last_error.clone())
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(prompt).style(base_style(app)), lines[0]);

    frame.render_widget(
        Paragraph::new(status_bar_actions(app)).style(base_style(app).add_modifier(Modifier::BOLD)),
        lines[1],
    );
}

fn status_bar_actions(app: &AppState) -> Line<'static> {
    if app.active_view == ActiveView::Detail {
        return detail_footer_actions(app);
    }

    let footer_actions = format!(
        "[H]elp  [F]ilter /  [T]ree:{}  [S]ortBy  [C]olumns  [A]uth  [K]ill",
        app.view_mode.footer_label()
    );
    let footer = app.discovery_status.footer_summary().map_or_else(
        || footer_actions.clone(),
        |summary| format!("{summary}  |  {footer_actions}"),
    );
    Line::from(footer)
}

fn detail_footer_actions(app: &AppState) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, tab) in DETAIL_TABS.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::raw("│"));
            spans.push(Span::raw("  "));
        }
        let style = if idx == app.detail_tab {
            Style::default()
                .fg(background_color(app))
                .bg(carat_color(app))
                .add_modifier(Modifier::BOLD)
        } else {
            base_style(app).add_modifier(Modifier::BOLD)
        };
        spans.extend(
            detail_tab_label(tab)
                .spans
                .into_iter()
                .map(|span| span.style(style)),
        );
    }
    Line::from(spans)
}

const fn help_bindings() -> &'static [(&'static str, &'static str)] {
    &[
        ("q", "Quit, or close the active overlay"),
        ("Ctrl+C", "Quit immediately"),
        ("H / F1", "Open full help page"),
        ("t / F5", "Cycle Tree, Flat, and Primary view in overview"),
        ("s / F6", "Choose sort column in overview"),
        ("c / F7 / v", "Toggle and reorder visible overview columns"),
        (
            "a / F8",
            "Enter credentials for the selected overview server",
        ),
        (
            "k / F9",
            "Open the kill picker for the selected overview row",
        ),
        ("f or /", "Edit the overview filter (keeps existing text)"),
        ("Esc", "Back from detail/help or stop filter editing"),
        ("Enter", "Open detail view from overview"),
        ("Tab/Right", "Next detail panel"),
        ("Left", "Previous detail panel"),
        (
            "S / L / I / C / B / K",
            "Jump to Summary, Latency, Info Raw, Commandstats, Bigkeys, or Hotkeys in detail",
        ),
        (
            "Up/Down",
            "Move selection in overview or scroll detail panes with long content",
        ),
        ("?", "Toggle help overlay"),
        (
            "C / N",
            "Start CPU or NET hotkeys sampling while on Hotkeys",
        ),
        (
            "X",
            "Stop active Hotkeys sampling early, or reset the Hotkeys pane when idle/complete",
        ),
        (
            "r / R",
            "Refresh now, or rerun Bigkeys/Hotkeys while on those panes",
        ),
        ("F3", "Start search input in overview"),
        (
            "F4",
            "Start filter input in overview (clears existing filter)",
        ),
        (
            "Shift+Up/Down",
            "Reorder visible columns inside the column picker",
        ),
        (
            "h",
            "Toggle host rendering (auto hide when all hosts are the same)",
        ),
        (
            "/",
            "Start filter input in overview or the active detail pane",
        ),
        ("Backspace", "Delete filter character while editing"),
    ]
}

fn draw_help_overlay(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let width = area.width.saturating_mul(80) / 100;
    let height = area.height.saturating_mul(70) / 100;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    let text = help_bindings()
        .iter()
        .map(|(keys, description)| format!("{keys}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text)
            .style(base_style(app))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Help")
                    .style(base_style(app)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_sort_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let width = area.width.saturating_mul(45) / 100;
    let height = area.height.saturating_mul(55) / 100;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let columns = app.sortable_columns();
    let rows: Vec<Row<'_>> = columns
        .iter()
        .map(|column_key| {
            let direction = if *column_key == app.sort_by {
                format!(" ({})", sort_direction_symbol(app.sort_direction))
            } else {
                String::new()
            };
            let label = app
                .column_registry
                .column(column_key)
                .map_or_else(|| column_key.clone(), |column| column.header().to_string());
            Row::new(vec![Cell::from(format!("{label}{direction}"))])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(100)])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Sort By (Enter select, Esc cancel)")
                .style(base_style(app)),
        )
        .style(base_style(app))
        .row_highlight_style(
            Style::default()
                .fg(carat_color(app))
                .bg(background_color(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state =
        ratatui::widgets::TableState::default().with_selected(Some(app.sort_picker_index));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, popup, &mut state);
}

fn draw_column_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let width = area.width.saturating_mul(55) / 100;
    let height = area.height.saturating_mul(60) / 100;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let rows: Vec<Row<'_>> = app
        .available_overview_columns()
        .iter()
        .map(|column_key| {
            let checked = if app.is_column_visible(column_key) {
                "[x]"
            } else {
                "[ ]"
            };
            let label = app
                .column_registry
                .column(column_key)
                .map_or_else(|| column_key.clone(), |column| column.header().to_string());
            let suffix = app
                .column_auto_hidden_suffix(column_key)
                .unwrap_or_else(|| {
                    if column_key == &app.sort_by {
                        " (sort)"
                    } else {
                        ""
                    }
                });
            Row::new(vec![Cell::from(format!("{checked} {label}{suffix}"))])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(100)])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(column_picker_title(app))
                .style(base_style(app)),
        )
        .style(base_style(app))
        .row_highlight_style(
            Style::default()
                .fg(carat_color(app))
                .bg(background_color(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(column_picker_highlight_symbol(app));
    let mut state =
        ratatui::widgets::TableState::default().with_selected(Some(app.column_picker_index));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, popup, &mut state);
}

fn draw_kill_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let width = area.width.saturating_mul(45) / 100;
    let height = area.height.saturating_mul(60) / 100;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let signal_supported = selected_signal_supported(app);
    let rows: Vec<Row<'_>> = KillAction::ALL
        .iter()
        .map(|action| {
            let suffix = if action.is_signal() && !signal_supported {
                " (needs local process_id)"
            } else {
                ""
            };
            Row::new(vec![Cell::from(format!("{}{}", action.label(), suffix))])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(100)])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(kill_picker_title(app))
                .style(base_style(app)),
        )
        .style(base_style(app))
        .row_highlight_style(
            Style::default()
                .fg(carat_color(app))
                .bg(background_color(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state =
        ratatui::widgets::TableState::default().with_selected(Some(app.kill_picker_index));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, popup, &mut state);
}

fn draw_auth_form(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let Some(form) = &app.auth_form else {
        return;
    };
    let width = area.width.saturating_mul(60) / 100;
    let width = width.clamp(36, 72).min(area.width);
    let height = 8.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(3)])
        .split(popup);
    let target = app.instances.get(&form.target_key).map_or_else(
        || form.target_key.clone(),
        |instance| {
            instance
                .alias
                .clone()
                .unwrap_or_else(|| instance.addr.clone())
        },
    );
    let password_mask = "•".repeat(form.password.chars().count());
    let rows = vec![
        Row::new(vec![
            Cell::from("Username"),
            Cell::from(form.username.clone()),
        ]),
        Row::new(vec![Cell::from("Password"), Cell::from(password_mask)]),
    ];
    let table = Table::new(rows, [Constraint::Length(12), Constraint::Min(1)])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Authenticate {target}"))
                .style(base_style(app)),
        )
        .style(base_style(app))
        .row_highlight_style(
            Style::default()
                .fg(carat_color(app))
                .bg(background_color(app))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let selected = match form.active_field {
        AuthField::Username => 0,
        AuthField::Password => 1,
    };
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, chunks[0], &mut state);
    frame.render_widget(
        Paragraph::new(
            "Username defaults to default. Tab switches fields; Enter connects; Esc cancels.",
        )
        .style(base_style(app))
        .wrap(Wrap { trim: true }),
        chunks[1],
    );
}

fn kill_picker_title(app: &AppState) -> String {
    let selected = app
        .selected_key()
        .and_then(|key| app.instances.get(&key))
        .map_or_else(
            || "selected server".to_string(),
            |instance| {
                instance
                    .alias
                    .clone()
                    .unwrap_or_else(|| instance.addr.clone())
            },
        );
    format!("Kill {selected} (Enter select, Esc cancel)")
}

fn selected_signal_supported(app: &AppState) -> bool {
    let Some(key) = app.selected_key() else {
        return false;
    };
    let Some(instance) = app.instances.get(&key) else {
        return false;
    };
    instance.detail.process_id.is_some()
        && if instance.addr.contains('/') {
            true
        } else {
            tcp_host(&instance.addr).is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
        }
}

fn handle_column_picker_key(app: &mut AppState, key: KeyEvent) -> bool {
    match key.kind {
        KeyEventKind::Release => {
            if shift_modifier_key(key.code) {
                app.set_column_picker_reorder_mode(false);
            }
        }
        KeyEventKind::Press | KeyEventKind::Repeat => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
            KeyCode::Modifier(modifier) if is_shift_modifier(modifier) => {
                app.set_column_picker_reorder_mode(true);
            }
            KeyCode::Up => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.set_column_picker_reorder_mode(true);
                    app.move_selected_column(-1);
                } else {
                    app.set_column_picker_reorder_mode(false);
                    app.move_column_picker_selection(-1);
                }
            }
            KeyCode::Down => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.set_column_picker_reorder_mode(true);
                    app.move_selected_column(1);
                } else {
                    app.set_column_picker_reorder_mode(false);
                    app.move_column_picker_selection(1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => app.toggle_selected_column_visibility(),
            _ => {}
        },
    }
    true
}

const fn column_picker_title(app: &AppState) -> &'static str {
    if app.column_picker_reorder_mode {
        "Columns (Shift+Up/Down move, Enter/Space toggle, Esc close)"
    } else {
        "Columns (Enter/Space toggle, Shift+Up/Down move, Esc close)"
    }
}

const fn column_picker_highlight_symbol(app: &AppState) -> &'static str {
    if app.column_picker_reorder_mode {
        "↕ "
    } else {
        "> "
    }
}

const fn shift_modifier_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Modifier(
            ModifierKeyCode::LeftShift
                | ModifierKeyCode::RightShift
                | ModifierKeyCode::IsoLevel3Shift
                | ModifierKeyCode::IsoLevel5Shift
        )
    )
}

const fn is_shift_modifier(modifier: ModifierKeyCode) -> bool {
    matches!(
        modifier,
        ModifierKeyCode::LeftShift
            | ModifierKeyCode::RightShift
            | ModifierKeyCode::IsoLevel3Shift
            | ModifierKeyCode::IsoLevel5Shift
    )
}

fn base_style(app: &AppState) -> Style {
    Style::default()
        .fg(foreground_color(app))
        .bg(background_color(app))
}

const fn background_color(app: &AppState) -> Color {
    app.settings.ui_theme.background.to_ratatui_color()
}

const fn foreground_color(app: &AppState) -> Color {
    app.settings.ui_theme.foreground.to_ratatui_color()
}

const fn carat_color(app: &AppState) -> Color {
    app.settings.ui_theme.carat.to_ratatui_color()
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    use super::{
        background_color, bigkeys_age_title, carat_color, commandstats_page_len,
        compute_column_widths, detail_tab_index_for_shortcut, detail_tabs_widget, draw,
        draw_status_bar, format_aligned_rows, format_with_commas, handle_column_picker_key,
        handle_overlay_quit_key, handle_overview_shortcut, handle_primary_view_quit_key,
        help_bindings, is_force_quit_key, ratatui_color_from_cluster, selected_signal_supported,
    };
    use crate::app::{ActiveView, AppState, OverviewModal};
    use crate::column::{Align, CellText, Column, RenderCtx, SortCtx, SortKey, WidthHint};
    use crate::config::default_settings;
    use crate::model::{
        BigkeyEntry, BigkeysScanStatus, CommandStat, ErrorDetails, InstanceState, SortMode, Status,
        ViewMode,
    };
    use crate::overview::{cluster_color_for_token, fit_cell_text, render_plain_text};
    use crate::registry::ColumnRegistry;

    fn test_registry() -> ColumnRegistry {
        ColumnRegistry::load(None, true, SortMode::Address)
    }

    fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let width = usize::from(buffer.area.width);
        buffer
            .content()
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect()
    }

    fn char_column(line: &str, needle: &str) -> usize {
        let byte_idx = line.find(needle).expect("needle rendered in line");
        line[..byte_idx].chars().count()
    }

    #[test]
    fn format_with_commas_groups_digits() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(12), "12");
        assert_eq!(format_with_commas(1_234), "1,234");
        assert_eq!(format_with_commas(12_345_678), "12,345,678");
    }

    #[test]
    fn format_aligned_rows_uses_consistent_label_column() {
        let body = format_aligned_rows(&[("a", "1".to_string()), ("long_name", "2".to_string())]);
        assert_eq!(body, "a         : 1\nlong_name : 2");
    }

    #[test]
    fn help_bindings_include_help_page_shortcut() {
        assert!(help_bindings().iter().any(|(keys, _)| *keys == "H / F1"));
    }

    #[test]
    fn help_bindings_include_hotkeys_stop_or_reset_shortcut() {
        assert!(help_bindings().iter().any(|(keys, _)| *keys == "X"));
    }

    #[test]
    fn help_bindings_include_mnemonics_and_legacy_aliases() {
        for binding in [
            "H / F1",
            "F3",
            "F4",
            "t / F5",
            "s / F6",
            "c / F7 / v",
            "a / F8",
            "k / F9",
            "f or /",
        ] {
            assert!(help_bindings().iter().any(|(keys, _)| *keys == binding));
        }
    }

    #[test]
    fn help_bindings_describe_three_view_cycle() {
        assert!(
            help_bindings()
                .iter()
                .any(|(keys, description)| *keys == "t / F5"
                    && description.contains("Tree, Flat, and Primary"))
        );
    }

    #[test]
    fn overview_picker_shortcuts_open_expected_modal() {
        for (keys, modal) in [
            (
                vec![KeyCode::Char('s'), KeyCode::Char('S'), KeyCode::F(6)],
                OverviewModal::SortPicker,
            ),
            (
                vec![
                    KeyCode::Char('c'),
                    KeyCode::Char('C'),
                    KeyCode::Char('v'),
                    KeyCode::F(7),
                ],
                OverviewModal::ColumnPicker,
            ),
            (
                vec![KeyCode::Char('a'), KeyCode::Char('A'), KeyCode::F(8)],
                OverviewModal::AuthForm,
            ),
            (
                vec![KeyCode::Char('k'), KeyCode::Char('K'), KeyCode::F(9)],
                OverviewModal::KillPicker,
            ),
        ] {
            for code in keys {
                let mut app = AppState::new(default_settings(), test_registry());
                app.apply_update(InstanceState::new("server".into(), "127.0.0.1:6379".into()));
                let sort_before = app.sort_by.clone();
                assert!(handle_overview_shortcut(
                    &mut app,
                    KeyEvent::new(code, KeyModifiers::NONE)
                ));
                assert_eq!(app.overview_modal, modal, "{code:?}");
                assert_eq!(
                    app.sort_by, sort_before,
                    "opening Sort By must not change the sort"
                );
            }
        }
    }

    #[test]
    fn overview_filter_shortcuts_preserve_text_and_legacy_clear_behavior() {
        for code in [
            KeyCode::Char('f'),
            KeyCode::Char('F'),
            KeyCode::Char('/'),
            KeyCode::F(3),
            KeyCode::F(4),
        ] {
            let mut app = AppState::new(default_settings(), test_registry());
            app.filter = "redis".into();
            assert!(handle_overview_shortcut(
                &mut app,
                KeyEvent::new(code, KeyModifiers::NONE)
            ));
            assert!(app.is_filtering);
            assert_eq!(app.filter, if code == KeyCode::F(4) { "" } else { "redis" });
            assert_eq!(
                app.filter_prompt_mode,
                if code == KeyCode::F(3) {
                    crate::app::FilterPromptMode::Search
                } else {
                    crate::app::FilterPromptMode::Filter
                }
            );
        }
    }

    #[test]
    fn overview_tree_shortcuts_cycle_all_modes() {
        for code in [KeyCode::Char('t'), KeyCode::Char('T'), KeyCode::F(5)] {
            let mut app = AppState::new(default_settings(), test_registry());
            for expected in [ViewMode::Flat, ViewMode::Primary, ViewMode::Tree] {
                assert!(handle_overview_shortcut(
                    &mut app,
                    KeyEvent::new(code, KeyModifiers::NONE)
                ));
                assert_eq!(app.view_mode, expected);
            }
        }
    }

    #[test]
    fn overview_shortcuts_do_not_interfere_with_other_input_contexts() {
        let mut app = AppState::new(default_settings(), test_registry());
        let shortcuts = [
            'a', 'A', 'f', 'F', '/', 't', 'T', 's', 'S', 'c', 'C', 'k', 'K',
        ];
        for view in [ActiveView::Detail, ActiveView::Help] {
            app.active_view = view;
            for ch in shortcuts {
                assert!(!handle_overview_shortcut(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
                ));
            }
        }
        app.active_view = ActiveView::Overview;
        for modal in [
            OverviewModal::SortPicker,
            OverviewModal::ColumnPicker,
            OverviewModal::AuthForm,
            OverviewModal::KillPicker,
        ] {
            app.overview_modal = modal;
            for ch in shortcuts {
                assert!(!handle_overview_shortcut(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
                ));
                assert_eq!(app.overview_modal, modal);
            }
        }
        app.overview_modal = OverviewModal::None;
        app.is_filtering = true;
        for ch in shortcuts {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
            ));
        }
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert_eq!(app.view_mode, ViewMode::Tree);
    }

    #[test]
    fn overview_commands_require_plain_key_press() {
        let mut app = AppState::new(default_settings(), test_registry());
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Char('k'), KeyModifiers::NONE, kind)
            ));
        }
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), modifiers)
            ));
        }
        assert!(handle_overview_shortcut(
            &mut app,
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)
        ));
        assert_eq!(app.overview_modal, OverviewModal::ColumnPicker);
    }

    #[test]
    fn auth_and_kill_shortcuts_require_a_selected_server() {
        let mut app = AppState::new(default_settings(), test_registry());
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('k'),
            KeyCode::F(8),
            KeyCode::F(9),
        ] {
            assert!(handle_overview_shortcut(
                &mut app,
                KeyEvent::new(code, KeyModifiers::NONE)
            ));
            assert_eq!(app.overview_modal, OverviewModal::None);
            assert!(app.auth_form.is_none());
        }
    }

    #[test]
    fn detail_tab_shortcuts_match_expected_tabs_case_insensitively() {
        assert_eq!(detail_tab_index_for_shortcut('s'), Some(0));
        assert_eq!(detail_tab_index_for_shortcut('L'), Some(1));
        assert_eq!(detail_tab_index_for_shortcut('i'), Some(2));
        assert_eq!(detail_tab_index_for_shortcut('C'), Some(3));
        assert_eq!(detail_tab_index_for_shortcut('b'), Some(4));
        assert_eq!(detail_tab_index_for_shortcut('k'), Some(5));
        assert_eq!(detail_tab_index_for_shortcut('x'), None);
    }

    #[test]
    fn force_quit_key_matches_ctrl_c() {
        assert!(is_force_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn force_quit_key_does_not_match_plain_c_or_q() {
        assert!(!is_force_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
        assert!(!is_force_quit_key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn q_closes_help_overlay_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.show_help = true;

        let handled = handle_overlay_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(handled);
        assert!(!app.show_help);
    }

    #[test]
    fn esc_closes_help_overlay_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.show_help = true;

        let handled =
            handle_overlay_quit_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(handled);
        assert!(!app.show_help);
    }

    #[test]
    fn q_closes_sort_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::SortPicker;

        let handled = handle_overlay_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn esc_closes_sort_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::SortPicker;

        let handled =
            handle_overlay_quit_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn q_closes_column_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::ColumnPicker;
        app.column_picker_reorder_mode = true;

        let handled = handle_overlay_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert!(!app.column_picker_reorder_mode);
    }

    #[test]
    fn q_closes_kill_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::KillPicker;

        let handled = handle_overlay_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn esc_closes_kill_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::KillPicker;

        let handled =
            handle_overlay_quit_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn esc_closes_column_picker_instead_of_quitting() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::ColumnPicker;
        app.column_picker_reorder_mode = true;

        let handled =
            handle_overlay_quit_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(handled);
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert!(!app.column_picker_reorder_mode);
    }

    #[test]
    fn q_without_overlay_is_not_handled_as_overlay_quit() {
        let mut app = AppState::new(default_settings(), test_registry());

        let handled = handle_overlay_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(!handled);
    }

    #[test]
    fn q_quits_from_overview_without_overlay() {
        let mut app = AppState::new(default_settings(), test_registry());

        let handled = handle_primary_view_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(handled);
        assert!(app.should_quit);
    }

    #[test]
    fn esc_quits_from_overview_without_overlay() {
        let mut app = AppState::new(default_settings(), test_registry());

        let handled =
            handle_primary_view_quit_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(handled);
        assert!(app.should_quit);
    }

    #[test]
    fn primary_view_quit_is_not_handled_when_overlay_is_open() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.overview_modal = OverviewModal::SortPicker;

        let handled = handle_primary_view_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(!handled);
        assert!(!app.should_quit);
    }

    #[test]
    fn primary_view_quit_is_not_handled_outside_overview() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;

        let handled = handle_primary_view_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(!handled);
        assert!(!app.should_quit);
    }

    #[test]
    fn fit_cell_text_right_aligns_headers_and_values_consistently() {
        assert_eq!(fit_cell_text("Ops/s", 8, Align::Right), "   Ops/s");
        assert_eq!(fit_cell_text("123", 8, Align::Right), "     123");
    }

    struct TestColumn {
        hint: WidthHint,
    }

    impl Column for TestColumn {
        fn header(&self) -> &'static str {
            ""
        }

        fn align(&self) -> Align {
            Align::Left
        }

        fn width_hint(&self) -> WidthHint {
            self.hint
        }

        fn render_cell(&self, _ctx: &RenderCtx<'_>) -> CellText {
            CellText::plain(String::new())
        }

        fn sort_key(&self, _ctx: &SortCtx<'_>) -> SortKey {
            SortKey::Null
        }
    }

    #[test]
    fn compute_column_widths_reserves_space_for_spacing() {
        let a: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 5,
                ideal: 8,
                max: None,
                fixed: None,
            },
        });
        let b: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 5,
                ideal: 8,
                max: None,
                fixed: None,
            },
        });
        let c: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 5,
                ideal: 8,
                max: None,
                fixed: None,
            },
        });
        let columns = vec![&a, &b, &c];

        let widths = compute_column_widths(20, &columns, 1);

        assert_eq!(widths, vec![6, 6, 6]);
        assert_eq!(widths.iter().sum::<u16>() + 2, 20);
    }

    #[test]
    fn compute_column_widths_shrinks_below_min_when_required() {
        let a: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 4,
                ideal: 4,
                max: None,
                fixed: None,
            },
        });
        let b: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 4,
                ideal: 4,
                max: None,
                fixed: None,
            },
        });
        let c: Arc<dyn Column> = Arc::new(TestColumn {
            hint: WidthHint {
                min: 4,
                ideal: 4,
                max: None,
                fixed: None,
            },
        });
        let columns = vec![&a, &b, &c];

        let widths = compute_column_widths(8, &columns, 1);

        assert_eq!(widths.iter().sum::<u16>() + 2, 8);
        assert!(widths.iter().all(|width| *width >= 1));
    }

    #[test]
    fn column_picker_shift_modifier_toggles_reorder_mode_on_press_and_release() {
        let mut app = AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.open_column_picker();

        assert!(handle_column_picker_key(
            &mut app,
            KeyEvent::new_with_kind(
                KeyCode::Modifier(ModifierKeyCode::LeftShift),
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ),
        ));
        assert!(app.column_picker_reorder_mode);

        assert!(handle_column_picker_key(
            &mut app,
            KeyEvent::new_with_kind(
                KeyCode::Modifier(ModifierKeyCode::LeftShift),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        ));
        assert!(!app.column_picker_reorder_mode);
    }

    #[test]
    fn overview_renders_cluster_gutter_with_stable_color() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.cluster_id = Some("cluster-b".into());
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.cluster_id = Some("cluster-a".into());
        b.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);
        app.selected_index = 1;

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("overview draw succeeds");

        let buffer = terminal.backend().buffer().clone();
        let lines = buffer_lines(&buffer);
        let row = lines
            .iter()
            .position(|line| line.contains("6379"))
            .expect("cluster row rendered");
        let width = usize::from(buffer.area.width);
        let row_start = row * width;
        let row_end = row_start + width;
        let gutter_cell = buffer.content()[row_start..row_end]
            .iter()
            .find(|cell| {
                cell.symbol() == "│"
                    && cell.fg == ratatui_color_from_cluster(cluster_color_for_token("2"))
            })
            .expect("cluster gutter cell rendered with logical-cluster color");

        assert_eq!(
            gutter_cell.fg,
            ratatui_color_from_cluster(cluster_color_for_token("2")),
            "gutter color should be derived from the logical cluster label"
        );
    }

    #[test]
    fn status_bar_shows_active_view_mode_label() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.view_mode = ViewMode::Primary;

        let backend = TestBackend::new(100, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_status_bar(frame, &app, frame.area()))
            .expect("status bar draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("[T]ree:Primary")));
        assert!(lines.iter().any(|line| line.contains("[A]uth")));
        assert!(lines.iter().any(|line| line.contains("[K]ill")));
        for label in ["[H]elp", "[F]ilter /", "[S]ortBy", "[C]olumns"] {
            assert!(lines.iter().any(|line| line.contains(label)));
        }
    }

    #[test]
    fn auth_form_masks_password() {
        let mut app = crate::app::AppState::new(default_settings(), test_registry());
        app.apply_update(InstanceState::new(
            "127.0.0.1:6380".into(),
            "127.0.0.1:6380".into(),
        ));
        app.open_auth_form();
        let form = app.auth_form.as_mut().expect("auth form should open");
        form.password = "secret".to_string();
        form.active_field = crate::app::AuthField::Password;

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("auth form draw succeeds");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Authenticate 127.0.0.1:6380"));
        assert!(rendered.contains("••••••"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn selected_signal_supported_requires_local_process_id() {
        let mut app = crate::app::AppState::new(default_settings(), test_registry());
        let mut local = InstanceState::new("127.0.0.1:6379".into(), "127.0.0.1:6379".into());
        local.detail.process_id = Some(42);
        app.apply_update(local);

        assert!(selected_signal_supported(&app));

        let mut remote =
            InstanceState::new("redis.example:6379".into(), "redis.example:6379".into());
        remote.detail.process_id = Some(42);
        app.instances.insert(remote.key.clone(), remote);
        app.selected_index = 1;

        assert!(!selected_signal_supported(&app));
    }

    #[test]
    fn status_bar_shows_detail_tabs_in_detail_view() {
        let mut app = crate::app::AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = 4;

        let backend = TestBackend::new(100, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_status_bar(frame, &app, frame.area()))
            .expect("status bar draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("[S]ummary")));
        assert!(lines.iter().any(|line| line.contains("[L]atency")));
        assert!(lines.iter().any(|line| line.contains("[I]nfo Raw")));
        assert!(lines.iter().any(|line| line.contains("[C]ommandstats")));
        assert!(lines.iter().any(|line| line.contains("[B]igkeys")));
        assert!(lines.iter().any(|line| line.contains("Hot[k]eys")));
    }

    #[test]
    fn detail_tabs_render_shortcuts_and_highlight_selected_tab() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.detail_tab = 1;

        let backend = TestBackend::new(100, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(detail_tabs_widget(&app), frame.area()))
            .expect("tab draw succeeds");

        let buffer = terminal.backend().buffer().clone();
        let lines = buffer_lines(&buffer);
        assert!(lines.iter().any(|line| line.contains("[S]ummary")));
        assert!(lines.iter().any(|line| line.contains("[L]atency")));
        assert!(lines.iter().any(|line| line.contains("[I]nfo Raw")));
        assert!(lines.iter().any(|line| line.contains("[C]ommandstats")));
        assert!(lines.iter().any(|line| line.contains("[B]igkeys")));
        assert!(lines.iter().any(|line| line.contains("Hot[k]eys")));

        let line_index = lines
            .iter()
            .position(|line| line.contains("[L]atency"))
            .expect("latency tab rendered");
        let tab_row = &lines[line_index];
        let width = usize::from(buffer.area.width);
        let latency_col = char_column(tab_row, "[L]atency");
        let latency_idx = line_index * width + latency_col;

        assert_eq!(buffer.content()[latency_idx].symbol(), "[");
        assert_eq!(buffer.content()[latency_idx].fg, background_color(&app));
        assert_eq!(buffer.content()[latency_idx].bg, carat_color(&app));
        assert!(
            buffer.content()[latency_idx]
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn detail_commandstats_tab_renders_sorted_table() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 3;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = vec![
            CommandStat {
                command: "echo".into(),
                calls: 2_057,
                usec: 49_361_425,
                usec_per_call: 23_996.80,
            },
            CommandStat {
                command: "lrange".into(),
                calls: 400_000,
                usec: 6_420_146,
                usec_per_call: 16.05,
            },
        ];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Commandstats")));
        assert!(lines.iter().any(|line| line.contains("Command")));
        assert!(lines.iter().any(|line| line.contains("Usec/Call")));

        let lrange_row = lines
            .iter()
            .position(|line| line.contains("lrange"))
            .expect("lrange row rendered");
        let echo_row = lines
            .iter()
            .position(|line| line.contains("echo"))
            .expect("echo row rendered");
        assert!(
            lrange_row < echo_row,
            "rows should be sorted by calls descending"
        );
    }

    #[test]
    fn detail_info_raw_tab_filters_lines_and_shows_filter_in_title() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 2;
        app.info_raw_view.filter = "run_id".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.raw_info =
            Some("# Server\nredis_version:8.0.0\nrun_id:abc123\nprocess_id:42".into());
        app.apply_update(instance);

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Info Raw 1-1 / 1")));
        assert!(lines.iter().any(|line| line.contains("filter=/run_id")));
        assert!(lines.iter().any(|line| line.contains("run_id:abc123")));
        assert!(!lines.iter().any(|line| line.contains("process_id:42")));
    }

    #[test]
    fn detail_summary_tab_pages_lines_with_scroll_offset() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 0;
        app.summary_view.scroll_offset = 2;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.used_memory_bytes = Some(1_024);
        instance.maxmemory_bytes = Some(4_096);
        instance.ops_per_sec = Some(9);
        instance.detail.used_memory_rss = Some(2_048);
        instance.detail.total_commands_processed = Some(11);
        instance.detail.connected_clients = Some(12);
        instance.detail.blocked_clients = Some(13);
        instance.detail.keyspace_hits = Some(14);
        instance.detail.keyspace_misses = Some(15);
        instance.detail.evicted_keys = Some(16);
        instance.detail.expired_keys = Some(17);
        app.apply_update(instance);

        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Summary 3-9 / 14")));
        assert!(!lines.iter().any(|line| line.contains("status           :")));
        assert!(!lines.iter().any(|line| line.contains("used_memory      :")));
        assert!(lines.iter().any(|line| line.contains("used_memory_rss")));
        assert!(lines.iter().any(|line| line.contains("commands")));
        assert!(!lines.iter().any(|line| line.contains("master           :")));
    }

    #[test]
    fn detail_commandstats_tab_filters_rows_and_shows_filter_in_title() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 3;
        app.commandstats_view.filter = "ran".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = vec![
            CommandStat {
                command: "echo".into(),
                calls: 2_057,
                usec: 49_361_425,
                usec_per_call: 23_996.80,
            },
            CommandStat {
                command: "lrange".into(),
                calls: 400_000,
                usec: 6_420_146,
                usec_per_call: 16.05,
            },
        ];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("filter=/ran")));
        assert!(lines.iter().any(|line| line.contains("lrange")));
        assert!(!lines.iter().any(|line| line.contains("echo")));
    }

    #[test]
    fn detail_commandstats_tab_pages_rows_with_scroll_offset() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 3;
        app.commandstats_view.scroll_offset = 2;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = (0..8)
            .map(|idx| CommandStat {
                command: format!("cmd{idx}"),
                calls: u64::try_from(100 - idx).expect("non-negative"),
                usec: 10,
                usec_per_call: 1.0,
            })
            .collect();
        app.apply_update(instance);

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Commandstats 2-8 / 8"))
        );
        assert!(!lines.iter().any(|line| line.contains("cmd0")));
        assert!(lines.iter().any(|line| line.contains("cmd1")));
        assert!(lines.iter().any(|line| line.contains("cmd2")));
        assert!(lines.iter().any(|line| line.contains("cmd5")));
        assert!(lines.iter().any(|line| line.contains("cmd7")));
        assert_eq!(commandstats_page_len(10), 4);
    }

    #[test]
    fn detail_bigkeys_tab_renders_single_panel_and_rows() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 4;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.bigkeys.status = BigkeysScanStatus::Ready;
        instance.detail.bigkeys.last_completed = Some(std::time::Instant::now());
        instance.detail.bigkeys.largest_keys = vec![
            BigkeyEntry {
                key: "sessions".into(),
                key_type: "hash".into(),
                size: Some(2_048),
                memory_usage: Some(65_536),
            },
            BigkeyEntry {
                key: "timeline".into(),
                key_type: "list".into(),
                size: Some(1_024),
                memory_usage: Some(32_768),
            },
        ];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Bigkeys")));
        assert!(lines.iter().any(|line| line.contains("Length")));
        assert!(lines.iter().any(|line| line.contains("age: 0s")));
        assert!(lines.iter().any(|line| line.contains("sessions")));
        assert!(lines.iter().any(|line| line.contains("timeline")));
        assert!(lines.iter().any(|line| line.contains("64 KiB")));
        assert!(!lines.iter().any(|line| line.contains("65,536 (64 KiB)")));
    }

    #[test]
    fn detail_bigkeys_tab_filters_rows_and_shows_filter_in_title() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 4;
        app.bigkeys_view.filter = "hash".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.bigkeys.status = BigkeysScanStatus::Ready;
        instance.detail.bigkeys.last_completed = Some(std::time::Instant::now());
        instance.detail.bigkeys.largest_keys = vec![
            BigkeyEntry {
                key: "sessions".into(),
                key_type: "hash".into(),
                size: Some(2_048),
                memory_usage: Some(65_536),
            },
            BigkeyEntry {
                key: "timeline".into(),
                key_type: "list".into(),
                size: Some(1_024),
                memory_usage: Some(32_768),
            },
        ];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("filter=/hash")));
        assert!(lines.iter().any(|line| line.contains("sessions")));
        assert!(!lines.iter().any(|line| line.contains("timeline")));
    }

    #[test]
    fn detail_bigkeys_tab_shows_empty_state_for_filter_without_matches() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 4;
        app.bigkeys_view.filter = "nomatch".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.bigkeys.status = BigkeysScanStatus::Ready;
        instance.detail.bigkeys.last_completed = Some(std::time::Instant::now());
        instance.detail.bigkeys.largest_keys = vec![BigkeyEntry {
            key: "sessions".into(),
            key_type: "hash".into(),
            size: Some(2_048),
            memory_usage: Some(65_536),
        }];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("filter=/nomatch")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("No keys match the current filter"))
        );
    }

    #[test]
    fn detail_hotkeys_tab_renders_idle_prompt() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 5;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        app.apply_update(instance);

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Start sampling (60 seconds)"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[C] CPU [N] NET") || line.contains("C] CPU [N] NET"))
        );
        assert!(!lines.iter().any(|line| line.contains("[X]")));
    }

    #[test]
    fn detail_hotkeys_tab_renders_table_and_filter() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 5;
        app.hotkeys_view.filter = "alp".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.hotkeys.status = crate::hotkeys::HotkeysStatus::Ready;
        instance.detail.hotkeys.selected_metric = Some(crate::hotkeys::HotkeysMetric::Cpu);
        instance.detail.hotkeys.total_value = Some(100);
        instance.detail.hotkeys.last_completed = Some(std::time::Instant::now());
        instance.detail.hotkeys.entries = vec![
            crate::hotkeys::HotkeyEntry {
                key: "alpha".into(),
                value: 50,
            },
            crate::hotkeys::HotkeyEntry {
                key: "beta".into(),
                value: 25,
            },
        ];
        app.apply_update(instance);

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Hotkeys CPU")));
        assert!(lines.iter().any(|line| line.contains("filter=/alp")));
        assert!(lines.iter().any(|line| line.contains("CPU us")));
        assert!(lines.iter().any(|line| line.contains("50.00%")));
        assert!(lines.iter().any(|line| line.contains("alpha")));
        assert!(!lines.iter().any(|line| line.contains("beta")));
    }

    #[test]
    fn detail_hotkeys_sampling_renders_concise_body() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 5;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.hotkeys.status = crate::hotkeys::HotkeysStatus::Running;
        instance.detail.hotkeys.selected_metric = Some(crate::hotkeys::HotkeysMetric::Cpu);
        instance.detail.hotkeys.started_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(11))
                .expect("instant subtraction stays in range"),
        );
        instance.detail.hotkeys.finishes_at =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(49));
        app.apply_update(instance);

        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("Hotkeys CPU")));
        assert!(lines.iter().any(|line| line.contains("Sampling 49s")));
        assert!(lines.iter().any(|line| line.contains("Press [X] to stop")));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Time remaining: 49s"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Sampling hotkeys..."))
        );
    }

    #[test]
    fn bigkeys_age_title_hides_while_running() {
        let mut bigkeys = crate::model::BigkeysMetrics {
            status: BigkeysScanStatus::Running,
            last_completed: Some(std::time::Instant::now()),
            ..crate::model::BigkeysMetrics::default()
        };
        assert!(bigkeys_age_title(&bigkeys).is_none());

        bigkeys.status = BigkeysScanStatus::Ready;
        assert_eq!(
            bigkeys_age_title(&bigkeys).map(|line| line.to_string()),
            Some("age: 0s".to_string())
        );
    }

    #[test]
    fn detail_summary_renders_full_error_details() {
        let mut app = crate::app::AppState::new(default_settings(), test_registry());
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = 0;

        let mut instance = InstanceState::new("a".into(), "192.168.0.174:6379".into());
        instance.status = Status::Protected;
        instance.last_updated = Some(std::time::Instant::now());
        instance.error_details = Some(ErrorDetails {
            summary: "Redis protected mode denies remote connections".into(),
            message: "DENIED Redis is running in protected mode because protected mode is enabled and no password is set.".into(),
        });
        instance.last_error = instance
            .error_details
            .as_ref()
            .map(|details| details.summary.clone());
        app.apply_update(instance);

        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("status") && line.contains("PROTECTED"))
        );
        assert!(lines.iter().any(|line| line.contains("error_summary")));
        assert!(
            lines
                .iter()
                .any(|line| { line.contains("Redis protected mode denies remote connections") })
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("DENIED Redis is running in protected mode"))
        );
    }

    #[test]
    fn overview_renders_default_emphasis_without_underline() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.ops_per_sec = Some(4);
        a.last_latency_ms = Some(0.25);
        a.max_latency_ms = 1.4;
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.ops_per_sec = Some(99);
        b.last_latency_ms = Some(0.95);
        b.max_latency_ms = 0.8;
        b.status = Status::Ok;
        b.last_updated = Some(std::time::Instant::now());

        let mut c = InstanceState::new("c".into(), "127.0.0.1:6381".into());
        c.ops_per_sec = Some(3);
        c.last_latency_ms = Some(0.40);
        c.max_latency_ms = 2.1;
        c.status = Status::Ok;
        c.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);
        app.apply_update(c);

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("overview draw succeeds");

        let buffer = terminal.backend().buffer().clone();
        let lines = buffer_lines(&buffer);
        let ops_row = lines
            .iter()
            .position(|line| line.contains("6380") && line.contains("99"))
            .expect("ops winner row rendered");
        let ops_col = char_column(&lines[ops_row], "99");
        let lat_row = lines
            .iter()
            .position(|line| line.contains("6381") && line.contains("2.10"))
            .expect("latency max row rendered");
        let lat_col = char_column(&lines[lat_row], "2.10");
        let width = usize::from(buffer.area.width);
        let ops_idx = ops_row * width + ops_col;
        let lat_max_idx = lat_row * width + lat_col;

        assert!(
            !buffer.content()[ops_idx].modifier.contains(Modifier::BOLD),
            "ops winner should use the shipped non-bold default emphasis style"
        );
        assert!(
            !buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::BOLD),
            "latency max winner should use the shipped non-bold default emphasis style"
        );
        assert!(
            !buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::UNDERLINED),
            "latency max winner should not be underlined by default"
        );
    }

    #[test]
    fn overview_renders_configured_emphasis_modifiers_and_color() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[view.overview.emphasis_style]
italic = true
foreground_color = "yellow"

[columns.ops.emphasis_style]
underlined = true
"#,
        )
        .expect("write config");

        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(Some(&path), false, crate::model::SortMode::Address),
        );
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.ops_per_sec = Some(4);
        a.last_latency_ms = Some(0.25);
        a.max_latency_ms = 1.4;
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.ops_per_sec = Some(99);
        b.last_latency_ms = Some(0.95);
        b.max_latency_ms = 0.8;
        b.status = Status::Ok;
        b.last_updated = Some(std::time::Instant::now());

        let mut c = InstanceState::new("c".into(), "127.0.0.1:6381".into());
        c.ops_per_sec = Some(3);
        c.last_latency_ms = Some(0.40);
        c.max_latency_ms = 2.1;
        c.status = Status::Ok;
        c.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);
        app.apply_update(c);

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("overview draw succeeds");

        let buffer = terminal.backend().buffer().clone();
        let lines = buffer_lines(&buffer);
        let ops_row = lines
            .iter()
            .position(|line| line.contains("6380") && line.contains("99"))
            .expect("ops winner row rendered");
        let ops_col = char_column(&lines[ops_row], "99");
        let lat_row = lines
            .iter()
            .position(|line| line.contains("6381") && line.contains("2.10"))
            .expect("latency max row rendered");
        let lat_col = char_column(&lines[lat_row], "2.10");
        let width = usize::from(buffer.area.width);
        let ops_idx = ops_row * width + ops_col;
        let lat_max_idx = lat_row * width + lat_col;

        assert!(
            !buffer.content()[ops_idx].modifier.contains(Modifier::BOLD),
            "ops winner should preserve the default non-bold emphasis unless explicitly enabled"
        );
        assert!(
            buffer.content()[ops_idx]
                .modifier
                .contains(Modifier::UNDERLINED),
            "ops winner should apply per-column underline"
        );
        assert!(
            buffer.content()[ops_idx]
                .modifier
                .contains(Modifier::ITALIC),
            "ops winner should inherit global italic"
        );
        assert_eq!(buffer.content()[ops_idx].fg, Color::Yellow);

        assert!(
            buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::ITALIC),
            "latency winner should inherit global italic"
        );
        assert_eq!(buffer.content()[lat_max_idx].fg, Color::Yellow);
    }

    #[test]
    fn overview_only_flashes_latency_max_on_record_frames() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r"
[columns.lat_max.emphasis_style]
underlined = true
",
        )
        .expect("write config");

        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(Some(&path), false, crate::model::SortMode::Address),
        );
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.max_latency_ms = 1.4;
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.max_latency_ms = 2.1;
        b.status = Status::Ok;
        b.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("first overview draw succeeds");
        let first_buffer = terminal.backend().buffer().clone();
        let first_lines = buffer_lines(&first_buffer);
        let lat_row = first_lines
            .iter()
            .position(|line| line.contains("6380") && line.contains("2.10"))
            .expect("latency max row rendered");
        let lat_col = char_column(&first_lines[lat_row], "2.10");
        let width = usize::from(first_buffer.area.width);
        let lat_max_idx = lat_row * width + lat_col;
        assert!(
            first_buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::UNDERLINED),
            "record-setting frame should emphasize the new latency max"
        );

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("second overview draw succeeds");
        let second_buffer = terminal.backend().buffer().clone();
        assert!(
            !second_buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::UNDERLINED),
            "latency max emphasis should clear on the next frame without a new record"
        );

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.max_latency_ms = 2.6;
        b.status = Status::Ok;
        b.last_updated = Some(std::time::Instant::now());
        app.apply_update(b);

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("third overview draw succeeds");
        let third_buffer = terminal.backend().buffer().clone();
        let third_lines = buffer_lines(&third_buffer);
        let lat_row = third_lines
            .iter()
            .position(|line| line.contains("6380") && line.contains("2.60"))
            .expect("updated latency max row rendered");
        let lat_col = char_column(&third_lines[lat_row], "2.60");
        let width = usize::from(third_buffer.area.width);
        let lat_max_idx = lat_row * width + lat_col;
        assert!(
            third_buffer.content()[lat_max_idx]
                .modifier
                .contains(Modifier::UNDERLINED),
            "a later record should be emphasized again"
        );
    }

    #[test]
    fn render_overview_table_outputs_plain_text_table() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.alias = Some("alpha".into());
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.alias = Some("beta".into());
        b.status = Status::Down;
        b.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);

        let rendered = render_plain_text(&app.build_overview_frame());

        assert!(rendered.contains("Alias"));
        assert!(rendered.contains("Status"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("beta"));
        assert!(rendered.contains("DOWN"));
    }
}
