mod activity;
mod navigation;

use std::fmt::Write as _;
use std::io::{self, Stdout, Write};
use std::ops::Range;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::app::{
    ActiveView, AppState, AuthField, DetailTab, FilterPromptMode, OverviewModal, Scroll,
};
use crate::cli::{LaunchConfig, OutputMode};
use crate::column::{EmphasisStyle, Tone, format_bytes};
use crate::commandstats::{CommandstatsColumn, aggregate_commandstats};
use crate::discovery::{self, DiscoveryEvent};
use crate::hotkeys::{HotkeysMetric, HotkeysMetrics, HotkeysStatus};
use crate::model::{BigkeysMetrics, BigkeysScanStatus, CommandStat, InstanceState, KillAction};
use crate::overview::{OverviewHeader, fit_cell_text, render_plain_text, sort_direction_symbol};
use crate::poller::{self, PollerRequest, PollerUpdate};
use crate::registry::ColumnRegistry;
use crate::target_addr::is_local_addr;
use crate::text::truncate_chars;

pub async fn run(launch: LaunchConfig) -> Result<()> {
    if launch.verbose {
        eprintln!(
            "rtop: targets={} refresh={} connect_timeout={} command_timeout={}",
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

fn new_app(launch: &LaunchConfig) -> AppState {
    let registry = ColumnRegistry::load(
        launch.config_path.as_deref(),
        launch.no_default_config,
        launch.settings.default_sort,
    );
    AppState::new(launch.settings.clone(), registry)
}

/// Background sources feeding the app: the poller and host discovery.
struct Feeds {
    updates_rx: Receiver<PollerUpdate>,
    discovery_rx: Receiver<DiscoveryEvent>,
    request_tx: Sender<PollerRequest>,
}

impl Feeds {
    fn start(launch: LaunchConfig) -> Self {
        let (updates_rx, request_tx) =
            poller::start(launch.targets.clone(), launch.settings.clone());
        let discovery_rx = discovery::start(
            launch.discovery_targets,
            launch.discovery_seed_targets,
            launch.targets,
            launch.settings,
        );
        Self {
            updates_rx,
            discovery_rx,
            request_tx,
        }
    }

    fn drain_into(&mut self, app: &mut AppState) {
        while let Ok(update) = self.updates_rx.try_recv() {
            match update {
                PollerUpdate::State(state) => app.apply_update(*state),
                PollerUpdate::Remove { key } => app.remove_instance(&key),
                PollerUpdate::ResetStatsComplete { results } => {
                    if results.iter().all(|(_, result)| result.is_ok()) {
                        continue;
                    }
                    app.reset_stats_result = results
                        .into_iter()
                        .map(|(key, result)| match result {
                            Ok(()) => format!("{key}: statistics reset"),
                            Err(error) => format!("{key}: reset failed: {error}"),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if app.overview_modal == OverviewModal::ResetStatsResult {
                        app.popup_scroll.to_start();
                    }
                }
            }
        }
        while let Ok(event) = self.discovery_rx.try_recv() {
            app.apply_discovery_event(&event);
            if let DiscoveryEvent::VerificationSucceeded(verified) = event {
                let target = verified.target.clone();
                app.apply_verified_instance(*verified);
                self.send(PollerRequest::UpsertTarget(target));
            }
        }
    }

    /// Best effort: a full queue only delays a refresh the ticker will redo.
    fn send(&self, request: PollerRequest) {
        let _ = self.request_tx.try_send(request);
    }

    /// For user-initiated actions that must not be silently dropped.
    fn send_required(&self, request: PollerRequest, what: &str) -> Result<()> {
        self.request_tx
            .try_send(request)
            .map_err(|_| anyhow::anyhow!("Unable to queue {what} request"))
    }
}

async fn run_once(launch: LaunchConfig) -> Result<()> {
    let mut app = new_app(&launch);

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
    let mut app = new_app(&launch);
    let mut feeds = Feeds::start(launch);
    let mut frame_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        frame_interval.tick().await;
        feeds.drain_into(&mut app);
        let frame = app.build_overview_frame();
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &frame)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, launch: LaunchConfig) -> Result<()> {
    let mut app = new_app(&launch);
    let mut feeds = Feeds::start(launch);
    // A failed capability query falls back to repeat timing, just like a legacy terminal.
    let reports_release =
        cfg!(windows) || crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut navigation = navigation::Navigation::new(reports_release);

    loop {
        feeds.drain_into(&mut app);
        maybe_request_bigkeys_scan(&mut app, &feeds);

        terminal.draw(|frame| draw(frame, &mut app))?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                navigation.cancel_space_hold();
                continue;
            };
            if is_force_quit_key(key) {
                app.should_quit = true;
                continue;
            }
            if !navigation.handle_key(&mut app, key, std::time::Instant::now()) {
                handle_key(&mut app, key, &feeds)?;
            }
        }
        navigation.tick(&mut app, std::time::Instant::now());
    }

    Ok(())
}

/// Dispatches a key in priority order: text inputs, then modal dialogs,
/// then view-specific shortcuts, then global commands.
fn handle_key(app: &mut AppState, key: KeyEvent, feeds: &Feeds) -> Result<()> {
    if app.is_auth_form_open() {
        return handle_auth_form_key(app, key, feeds);
    }
    if handle_overlay_quit_key(app, key) {
        return Ok(());
    }
    if app.show_help {
        handle_help_key(app, key);
        return Ok(());
    }
    if app.is_filtering {
        handle_overview_filter_key(app, key);
        return Ok(());
    }
    if app.editing_pane().is_some() {
        let pane = app.active_pane_mut();
        edit_text_input(&mut pane.filter, &mut pane.is_filtering, key);
        return Ok(());
    }
    if app.is_sort_picker_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
            KeyCode::Enter => app.apply_sort_picker_selection(),
            _ => {}
        }
        return Ok(());
    }
    if app.is_column_picker_open() && handle_column_picker_key(app, key) {
        return Ok(());
    }
    if handle_kill_key(app, key, feeds)? {
        return Ok(());
    }
    if matches!(
        app.overview_modal,
        OverviewModal::ResetStatsConfirmation | OverviewModal::ResetStatsResult
    ) {
        scroll_popup(app, key);
        if key.kind == KeyEventKind::Press && !has_command_modifier(key) {
            if app.overview_modal == OverviewModal::ResetStatsConfirmation
                && matches!(key.code, KeyCode::Char('y' | 'Y'))
            {
                feeds.send_required(
                    PollerRequest::ResetStats {
                        keys: app.reset_stats_targets.clone(),
                    },
                    "reset statistics",
                )?;
                app.close_overview_modal();
            } else if key.code == KeyCode::Enter
                || (app.overview_modal == OverviewModal::ResetStatsConfirmation
                    && matches!(key.code, KeyCode::Char('n' | 'N')))
            {
                app.close_overview_modal();
            }
        }
        return Ok(());
    }
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    if handle_commandstats_shortcut(app, key) || handle_overview_shortcut(app, key) {
        return Ok(());
    }
    if app.active_view == ActiveView::Detail && handle_detail_key(app, key, feeds) {
        return Ok(());
    }

    match key.code {
        KeyCode::F(1) | KeyCode::Char('H' | '?') => {
            app.show_help = true;
            app.popup_scroll.to_start();
        }
        KeyCode::Char('r' | 'R') => feeds.send(PollerRequest::RefreshAll),
        KeyCode::Enter
            if app.active_view == ActiveView::Overview && app.selected_key().is_some() =>
        {
            app.active_view = ActiveView::Detail;
        }
        KeyCode::Char('q') | KeyCode::Esc if handle_primary_view_quit_key(app, key) => {}
        _ => {}
    }
    Ok(())
}

/// Shared line editor for filter prompts. Returns whether the key was consumed.
fn edit_text_input(text: &mut String, editing: &mut bool, key: KeyEvent) -> bool {
    // With enhanced keyboard reporting every key also produces a release event.
    if key.kind == KeyEventKind::Release {
        return false;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Enter => *editing = false,
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(ch) => text.push(ch),
        _ => return false,
    }
    true
}

fn handle_overview_filter_key(app: &mut AppState, key: KeyEvent) {
    let overview = app.active_view == ActiveView::Overview;
    match key.code {
        KeyCode::F(3) if overview => app.start_filter_input(FilterPromptMode::Search, false),
        KeyCode::F(4) if overview => app.start_filter_input(FilterPromptMode::Filter, true),
        _ => {
            if edit_text_input(&mut app.filter, &mut app.is_filtering, key) {
                app.clamp_selection();
            }
        }
    }
}

fn handle_auth_form_key(app: &mut AppState, key: KeyEvent, feeds: &Feeds) -> Result<()> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(());
    }
    match key.code {
        KeyCode::Esc => app.close_auth_form(),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => app.toggle_auth_field(),
        KeyCode::Enter => {
            let on_username = app
                .auth_form
                .as_ref()
                .is_some_and(|form| form.active_field == AuthField::Username);
            if on_username {
                app.toggle_auth_field();
            } else if let Some((keys, username, password)) = app.take_auth_credentials() {
                feeds.send_required(
                    PollerRequest::AuthenticateTargets {
                        keys,
                        username,
                        password,
                    },
                    "authentication",
                )?;
            }
        }
        KeyCode::Backspace => {
            if let Some(value) = app.auth_active_value_mut() {
                value.pop();
            }
        }
        KeyCode::Char(ch) => {
            if let Some(value) = app.auth_active_value_mut() {
                value.push(ch);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Keys that only mean something inside the detail view.
fn handle_detail_key(app: &mut AppState, key: KeyEvent, feeds: &Feeds) -> bool {
    let tab = app.detail_tab;
    match key.code {
        KeyCode::Char('/') => app.start_active_detail_filter_input(false),
        KeyCode::Esc => app.close_detail_view(),
        KeyCode::Tab => app.set_detail_tab(tab.rotate(1)),
        KeyCode::BackTab => app.set_detail_tab(tab.rotate(-1)),
        KeyCode::Char('c' | 'C') if tab == DetailTab::Hotkeys => {
            start_hotkeys_sampling(app, feeds, HotkeysMetric::Cpu);
        }
        KeyCode::Char('n' | 'N') if tab == DetailTab::Hotkeys => {
            start_hotkeys_sampling(app, feeds, HotkeysMetric::Net);
        }
        KeyCode::Char('x' | 'X') if tab == DetailTab::Hotkeys => {
            handle_hotkeys_stop_or_reset(app, feeds);
        }
        KeyCode::Char('r' | 'R') if tab == DetailTab::Hotkeys => {
            let metric = app
                .selected_instance()
                .and_then(|instance| instance.detail.hotkeys.selected_metric)
                .unwrap_or(HotkeysMetric::Cpu);
            start_hotkeys_sampling(app, feeds, metric);
        }
        KeyCode::Char('r' | 'R') if tab == DetailTab::Bigkeys => {
            if let Some(key) = app.selected_key() {
                mark_bigkeys_running(app, &key);
                feeds.send(PollerRequest::RefreshBigkeys { key, force: true });
            }
        }
        KeyCode::Char(ch) => match DetailTab::from_shortcut(ch) {
            Some(next) => app.set_detail_tab(next),
            None => return false,
        },
        _ => return false,
    }
    true
}

const fn has_command_modifier(key: KeyEvent) -> bool {
    key.modifiers.intersects(
        KeyModifiers::CONTROL
            .union(KeyModifiers::ALT)
            .union(KeyModifiers::SUPER),
    )
}

fn handle_commandstats_shortcut(app: &mut AppState, key: KeyEvent) -> bool {
    if !app.is_detail_tab(DetailTab::Commandstats)
        || app.editing_pane().is_some()
        || app.overview_modal != OverviewModal::None
        || app.show_help
        || key.kind != KeyEventKind::Press
        || has_command_modifier(key)
    {
        return false;
    }
    match key.code {
        KeyCode::Char('r' | 'R') => {
            app.reset_stats_targets = app.action_target_keys();
            if !app.reset_stats_targets.is_empty() {
                app.popup_scroll.to_start();
                app.overview_modal = OverviewModal::ResetStatsConfirmation;
            }
        }
        KeyCode::F(7) | KeyCode::Char('c' | 'C' | 'v' | 'V') => app.open_column_picker(),
        KeyCode::Char('p' | 'P') => {
            app.commandstats_compact = !app.commandstats_compact;
            app.active_pane_mut().scroll.to_start();
        }
        _ => return false,
    }
    true
}

/// Handle overview commands after text inputs and modal dialogs have consumed their keys.
fn handle_overview_shortcut(app: &mut AppState, key: KeyEvent) -> bool {
    if app.active_view != ActiveView::Overview
        || app.is_filtering
        || app.overview_modal != OverviewModal::None
        || app.show_help
        || key.kind != KeyEventKind::Press
        || has_command_modifier(key)
    {
        return false;
    }

    match key.code {
        KeyCode::Char(' ') => app.toggle_server_selection(),
        KeyCode::Char('m' | 'M') => app.show_activity = !app.show_activity,
        KeyCode::F(5) | KeyCode::Char('t' | 'T') => app.cycle_view_mode(),
        KeyCode::F(6) | KeyCode::Char('s' | 'S') => app.open_sort_picker(),
        KeyCode::F(7) | KeyCode::Char('c' | 'C' | 'v' | 'V') => app.open_column_picker(),
        KeyCode::F(8) | KeyCode::Char('a' | 'A') => app.open_auth_form(),
        KeyCode::F(9) | KeyCode::Char('K') => app.open_kill_picker(),
        KeyCode::Char('o' | 'O') => {
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

fn maybe_request_bigkeys_scan(app: &mut AppState, feeds: &Feeds) {
    if !app.is_detail_tab(DetailTab::Bigkeys) {
        return;
    }
    let Some(key) = app.selected_key() else {
        return;
    };
    let is_idle = app
        .instances
        .get(&key)
        .is_some_and(|instance| instance.detail.bigkeys.status == BigkeysScanStatus::Idle);
    if is_idle {
        mark_bigkeys_running(app, &key);
        feeds.send(PollerRequest::RefreshBigkeys { key, force: false });
    }
}

fn mark_bigkeys_running(app: &mut AppState, key: &str) {
    if let Some(instance) = app.instances.get_mut(key) {
        instance.detail.bigkeys.status = BigkeysScanStatus::Running;
        instance.detail.bigkeys.last_error = None;
    }
}

fn start_hotkeys_sampling(app: &mut AppState, feeds: &Feeds, metric: HotkeysMetric) {
    let Some(key) = app.selected_key() else {
        return;
    };

    app.clear_hotkeys_local_reset(&key);
    if let Some(instance) = app.instances.get_mut(&key) {
        instance
            .detail
            .hotkeys
            .start(metric, poller::HOTKEYS_DURATION);
    }
    feeds.send(PollerRequest::StartHotkeys {
        key,
        metric,
        force: true,
    });
}

fn handle_hotkeys_stop_or_reset(app: &mut AppState, feeds: &Feeds) {
    let Some(key) = app.selected_key() else {
        return;
    };

    let is_running = app
        .instances
        .get(&key)
        .is_some_and(|instance| instance.detail.hotkeys.status == HotkeysStatus::Running);

    if is_running {
        feeds.send(PollerRequest::StopHotkeys { key });
    } else {
        app.reset_hotkeys_locally(&key);
    }
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

const fn is_quit_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
}

fn handle_overlay_quit_key(app: &mut AppState, key: KeyEvent) -> bool {
    if !is_quit_press(key) {
        return false;
    }

    if app.show_help {
        app.show_help = false;
        return true;
    }

    if app.overview_modal != OverviewModal::None {
        app.close_overview_modal();
        return true;
    }

    false
}

fn handle_primary_view_quit_key(app: &mut AppState, key: KeyEvent) -> bool {
    if !is_quit_press(key)
        || app.active_view != ActiveView::Overview
        || app.is_filtering
        || app.show_help
        || app.overview_modal != OverviewModal::None
    {
        return false;
    }

    if key.code == KeyCode::Esc {
        // Clearing selection and exiting require separate presses.
        if key.kind == KeyEventKind::Repeat {
            return true;
        }
        if app.selected_server_count() > 0 {
            app.clear_server_selection();
            return true;
        }
    }

    app.should_quit = true;
    true
}

fn handle_kill_key(app: &mut AppState, key: KeyEvent, feeds: &Feeds) -> Result<bool> {
    if !matches!(
        app.overview_modal,
        OverviewModal::KillPicker | OverviewModal::KillConfirmation
    ) {
        return Ok(false);
    }
    // Repeated Enter events must not accept the additional confirmation.
    if key.kind != KeyEventKind::Press {
        return Ok(true);
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
        KeyCode::Enter => {
            if let Some((keys, action)) = app.submit_kill() {
                feeds.send_required(PollerRequest::KillTargets { keys, action }, "stop")?;
            }
        }
        _ => {}
    }
    Ok(true)
}

fn handle_column_picker_key(app: &mut AppState, key: KeyEvent) -> bool {
    match key.kind {
        KeyEventKind::Release => {
            if matches!(key.code, KeyCode::Modifier(modifier) if is_shift_modifier(modifier)) {
                app.set_column_picker_reorder_mode(false);
            }
        }
        KeyEventKind::Press | KeyEventKind::Repeat => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_overview_modal(),
            KeyCode::Modifier(modifier) if is_shift_modifier(modifier) => {
                app.set_column_picker_reorder_mode(true);
            }
            KeyCode::Enter | KeyCode::Char(' ') => app.toggle_selected_column_visibility(),
            _ => {}
        },
    }
    true
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

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut AppState) {
    if app.overview_modal == OverviewModal::None
        && !app.reset_stats_result.is_empty()
        && !app.show_help
        && !app.is_filtering
        && app.editing_pane().is_none()
    {
        app.overview_modal = OverviewModal::ResetStatsResult;
        app.popup_scroll.to_start();
    }
    app.sample_activity(std::time::Instant::now());
    let area = frame.area();
    frame.render_widget(Block::default().style(base_style(app)), area);
    let [main, status] = Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(area);

    match app.active_view {
        ActiveView::Overview => {
            let height = match (app.show_activity, main.height) {
                (true, 13..) => 6,
                (true, 10..) => 4,
                _ => 0,
            };
            let [activity_area, table] =
                Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(main);
            if height > 0 {
                activity::draw(frame, app, activity_area);
            }
            draw_overview(frame, app, table);
        }
        ActiveView::Detail => draw_detail(frame, app, main),
    }
    draw_status_bar(frame, app, status);

    match app.overview_modal {
        OverviewModal::None => {}
        OverviewModal::SortPicker => draw_sort_picker(frame, area, app),
        OverviewModal::ColumnPicker => draw_column_picker(frame, area, app),
        OverviewModal::KillPicker => draw_kill_picker(frame, area, app),
        OverviewModal::KillConfirmation => draw_kill_confirmation(frame, area, app),
        OverviewModal::ResetStatsConfirmation | OverviewModal::ResetStatsResult => {
            draw_reset_stats_popup(frame, area, app);
        }
        OverviewModal::AuthForm => draw_auth_form(frame, area, app),
    }

    if app.show_help {
        draw_help_overlay(frame, app, area);
    }
}

fn overview_cell(
    app: &AppState,
    fitted: String,
    emphasis_style: Option<EmphasisStyle>,
    tone: Option<Tone>,
) -> Cell<'static> {
    let mut style = emphasis_style.map_or_else(Style::default, style_from_emphasis);
    if let Some(tone) = tone
        && emphasis_style.is_none_or(|emphasis| emphasis.foreground.is_none())
    {
        style = style.fg(tone_color(app, tone));
    }
    Cell::from(Line::styled(fitted, style))
}

const fn tone_color(app: &AppState, tone: Tone) -> Color {
    let theme = &app.settings.ui_theme;
    match tone {
        Tone::Warning => theme.warning.to_ratatui_color(),
        Tone::Critical => theme.critical.to_ratatui_color(),
    }
}

fn overview_cluster_gutter_cell(
    cluster_gutter: Option<&crate::overview::OverviewClusterGutter>,
) -> Cell<'static> {
    cluster_gutter.map_or_else(
        || Cell::from(" "),
        |gutter| {
            Cell::from(Span::styled(
                "│",
                Style::default().fg(gutter.color.to_ratatui_color()),
            ))
        },
    )
}

fn style_from_emphasis(emphasis_style: EmphasisStyle) -> Style {
    let modifiers = [
        (emphasis_style.bold, Modifier::BOLD),
        (emphasis_style.italic, Modifier::ITALIC),
        (emphasis_style.underlined, Modifier::UNDERLINED),
        (emphasis_style.dim, Modifier::DIM),
        (emphasis_style.reversed, Modifier::REVERSED),
    ]
    .into_iter()
    .filter(|(enabled, _)| *enabled)
    .fold(Modifier::empty(), |all, (_, modifier)| all | modifier);
    let style = Style::default().add_modifier(modifiers);
    emphasis_style
        .foreground
        .map_or(style, |color| style.fg(color.to_ratatui_color()))
}

fn draw_overview(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    const TABLE_COLUMN_SPACING: u16 = 1;
    // Borders, shared focus/selection gutter, cluster gutter, and their two gaps.
    const TABLE_DECORATION_WIDTH: u16 = 2 + 1 + 1 + 2 * TABLE_COLUMN_SPACING;
    let overview = app.build_overview_frame();
    // Borders and the header row are not available for instance rows.
    app.overview_page_len = usize::from(area.height.saturating_sub(3));

    let columns: Vec<_> = overview
        .columns
        .iter()
        .filter_map(|column| app.column_registry.column(&column.key))
        .collect();
    let widths = compute_column_widths(
        area.width.saturating_sub(TABLE_DECORATION_WIDTH),
        &columns,
        TABLE_COLUMN_SPACING,
    );

    let table_rows: Vec<Row<'_>> = overview
        .rows
        .iter()
        .map(|row| {
            let mut cells = Vec::with_capacity(overview.columns.len() + 2);
            let marker = match (row.selected, app.is_server_selected(&row.key)) {
                (true, true) => "▶",
                (true, false) => ">",
                (false, true) => "●",
                (false, false) => " ",
            };
            let marker_style = if row.selected {
                base_style(app)
                    .fg(carat_color(app))
                    .add_modifier(Modifier::BOLD)
            } else {
                base_style(app)
            };
            cells.push(Cell::from(Span::styled(marker, marker_style)));
            cells.push(overview_cluster_gutter_cell(row.cluster_gutter.as_ref()));
            cells.extend(row.cells.iter().zip(&overview.columns).zip(&widths).map(
                |((cell, column), width)| {
                    let fitted = fit_cell_text(&cell.value, usize::from(*width), column.align);
                    let emphasis_style = cell.emphasized.then(|| {
                        column
                            .emphasis_style
                            .unwrap_or_else(|| app.column_registry.overview_emphasis_style())
                    });
                    overview_cell(app, fitted, emphasis_style, cell.tone)
                },
            ));

            let style = if row.stale {
                base_style(app).add_modifier(Modifier::DIM)
            } else {
                base_style(app)
            };
            Row::new(cells).style(style)
        })
        .collect();

    let constraints: Vec<Constraint> = [Constraint::Length(1), Constraint::Length(1)]
        .into_iter()
        .chain(widths.iter().copied().map(Constraint::Length))
        .collect();
    let header = Row::new([Cell::from(""), Cell::from(" ")].into_iter().chain(
        overview.columns.iter().zip(&widths).map(|(column, width)| {
            Cell::from(fit_cell_text(
                &column.label,
                usize::from(*width),
                column.align,
            ))
        }),
    ))
    .style(base_style(app).add_modifier(Modifier::BOLD));

    let block = bordered(
        app,
        format!("Overview ({} selected)", app.selected_server_count()),
    )
    .title(overview_status_line(app, &overview.header));
    let table = Table::new(table_rows, constraints)
        .header(header)
        .block(block)
        .column_spacing(TABLE_COLUMN_SPACING)
        .row_highlight_style(Style::default().bg(background_color(app)))
        .highlight_spacing(HighlightSpacing::Never);

    let mut state = TableState::default().with_selected(Some(app.selected_index));
    frame.render_stateful_widget(table, area, &mut state);
}

/// Right-aligned border title summarizing how the overview is configured.
fn overview_status_line(app: &AppState, header: &OverviewHeader) -> Line<'static> {
    let filter = if header.filter.is_empty() {
        "<none>"
    } else {
        &header.filter
    };
    let editing = if header.is_filtering {
        " (editing)"
    } else {
        ""
    };
    Line::from(format!(
        " refresh={}  view={}  sort={} {}  host={}  filter={filter}{editing} ",
        humantime::format_duration(app.settings.refresh_interval),
        app.view_mode.footer_label(),
        header.sort.label,
        sort_direction_symbol(header.sort.direction),
        header.host_rendering.label(),
    ))
    .right_aligned()
}

fn compute_column_widths(
    table_width: u16,
    columns: &[&std::sync::Arc<dyn crate::column::Column>],
    column_spacing: u16,
) -> Vec<u16> {
    if columns.is_empty() {
        return Vec::new();
    }

    let hints: Vec<_> = columns.iter().map(|column| column.width_hint()).collect();
    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let content_width = table_width.saturating_sub(column_spacing.saturating_mul(gaps));
    let mut widths: Vec<u16> = hints
        .iter()
        .map(|hint| hint.fixed.unwrap_or(hint.min))
        .collect();

    let used = widths.iter().copied().fold(0u16, u16::saturating_add);
    if used > content_width {
        shrink_widths_to_fit(&mut widths, content_width);
        return widths;
    }

    // Hand out the remaining space one cell at a time, round-robin, so every
    // flexible column approaches its ideal width evenly.
    let mut remaining = content_width - used;
    while remaining > 0 {
        let mut progressed = false;
        for (width, hint) in widths.iter_mut().zip(&hints) {
            if remaining == 0 {
                break;
            }
            let ideal = hint.ideal.min(hint.max.unwrap_or(u16::MAX));
            if hint.fixed.is_none() && *width < ideal {
                *width += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    widths
}

fn shrink_widths_to_fit(widths: &mut [u16], target: u16) {
    while widths.iter().copied().fold(0u16, u16::saturating_add) > target {
        let Some(widest) = widths.iter_mut().max_by_key(|width| **width) else {
            break;
        };
        if *widest <= 1 {
            break;
        }
        *widest -= 1;
    }
}

fn draw_detail(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    let commandstats_nodes: Vec<_> = if app.detail_tab == DetailTab::Commandstats {
        app.action_target_keys()
            .iter()
            .filter_map(|key| app.instances.get(key))
            .collect()
    } else {
        Vec::new()
    };
    let Some(instance) = commandstats_nodes
        .first()
        .copied()
        .or_else(|| app.selected_instance())
    else {
        frame.render_widget(
            Paragraph::new("No instance selected")
                .style(base_style(app))
                .block(bordered(app, "Detail")),
            area,
        );
        return;
    };

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).areas(area);

    let title = if commandstats_nodes.len() > 1 {
        format!(
            "Commandstats totals for {} selected nodes",
            commandstats_nodes.len()
        )
    } else {
        format!(
            "{} ({})  role={}  status={}  version={} uptime={}s",
            instance.display_name(),
            instance.addr,
            instance.kind.as_str(),
            instance.status.as_str(),
            instance.detail.redis_version.as_deref().unwrap_or("-"),
            format_optional_u64(instance.detail.uptime_seconds),
        )
    };
    frame.render_widget(
        Paragraph::new(title)
            .style(base_style(app))
            .block(bordered(app, "Instance")),
        header_area,
    );

    // Panes render from a copy of the scroll state and report back the
    // viewport they used, so input handling can clamp against it.
    let tab = app.detail_tab;
    let mut scroll = app.active_pane().scroll;
    let pane = PaneCtx {
        app: &*app,
        tab,
        area: body_area,
    };
    match tab {
        DetailTab::Summary | DetailTab::InfoRaw => {
            draw_detail_text(frame, &pane, &mut scroll, &detail_text_body(instance, tab));
        }
        DetailTab::Commandstats => {
            let stats = if commandstats_nodes.len() > 1 {
                std::borrow::Cow::Owned(aggregate_commandstats(
                    commandstats_nodes
                        .iter()
                        .flat_map(|node| &node.detail.commandstats),
                ))
            } else {
                std::borrow::Cow::Borrowed(instance.detail.commandstats.as_slice())
            };
            draw_commandstats(frame, &pane, &mut scroll, &stats, commandstats_nodes.len());
        }
        DetailTab::Bigkeys => draw_bigkeys(frame, &pane, &mut scroll, &instance.detail.bigkeys),
        DetailTab::Hotkeys => draw_hotkeys(frame, &pane, &mut scroll, &instance.detail.hotkeys),
    }
    app.active_pane_mut().scroll = scroll;
}

/// What a detail pane needs to render itself.
struct PaneCtx<'a> {
    app: &'a AppState,
    tab: DetailTab,
    area: Rect,
}

impl PaneCtx<'_> {
    /// Rows available inside the bordered block, minus `header_rows`.
    fn page_len(&self, header_rows: u16) -> usize {
        usize::from(self.area.height.saturating_sub(2 + header_rows))
    }

    /// `Base 1-10 / 42  filter=/text` style title for paged content.
    fn title(&self, base: &str, range: &Range<usize>, total: usize) -> String {
        let mut title = if total == 0 {
            base.to_string()
        } else {
            format!("{base} {}-{} / {total}", range.start + 1, range.end)
        };
        let filter = &self.app.pane(self.tab).filter;
        if !filter.is_empty() {
            let _ = write!(title, "  filter=/{filter}");
        }
        title
    }

    fn block(&self, title: String) -> Block<'static> {
        bordered(self.app, title)
    }

    fn message(&self, frame: &mut ratatui::Frame<'_>, block: Block<'static>, text: String) {
        self.aligned_message(frame, block, text, Alignment::Left);
    }

    fn aligned_message(
        &self,
        frame: &mut ratatui::Frame<'_>,
        block: Block<'static>,
        text: String,
        alignment: Alignment,
    ) {
        frame.render_widget(
            Paragraph::new(text)
                .style(base_style(self.app))
                .block(block)
                .alignment(alignment)
                .wrap(Wrap { trim: false }),
            self.area,
        );
    }

    fn table<'a>(
        &self,
        rows: Vec<Row<'a>>,
        widths: impl IntoIterator<Item = Constraint>,
        header: impl IntoIterator<Item = Cell<'a>>,
        block: Block<'a>,
    ) -> Table<'a> {
        Table::new(rows, widths)
            .header(Row::new(header).style(base_style(self.app).add_modifier(Modifier::BOLD)))
            .block(block)
            .style(base_style(self.app))
            .column_spacing(1)
    }
}

fn right(text: impl Into<String>) -> Cell<'static> {
    Cell::from(Line::from(text.into()).right_aligned())
}

fn draw_commandstats(
    frame: &mut ratatui::Frame<'_>,
    pane: &PaneCtx<'_>,
    scroll: &mut Scroll,
    stats: &[CommandStat],
    node_count: usize,
) {
    let app = pane.app;
    let visible = app.visible_commandstats(stats);
    let command_width = visible
        .iter()
        .map(|stat| Line::from(stat.command.as_str()).width())
        .max()
        .unwrap_or(0)
        .max(7);
    let calls_width = visible
        .iter()
        .map(|stat| format_with_commas(stat.calls).len())
        .max()
        .unwrap_or(0)
        .max(5);
    let pair_width = command_width + 1 + calls_width;
    let pairs = if app.commandstats_compact {
        ((usize::from(pane.area.width.saturating_sub(2)) + 2) / (pair_width + 2)).max(1)
    } else {
        1
    };
    let rows = scroll.viewport(visible.len().div_ceil(pairs), pane.page_len(1));
    let range = rows.start * pairs..(rows.end * pairs).min(visible.len());
    let mut title = pane.title("Commandstats", &range, visible.len());
    if node_count > 1 {
        let _ = write!(title, "  {node_count} nodes");
    }
    if app.commandstats_compact {
        title.push_str("  compact");
    }
    let block = pane.block(title);

    if stats.is_empty() {
        pane.message(frame, block, "INFO COMMANDSTATS not available".into());
        return;
    }
    if visible.is_empty() {
        pane.message(
            frame,
            block,
            "No commandstats match the current filter".into(),
        );
        return;
    }

    if app.commandstats_compact {
        let rows = visible[range]
            .chunks(pairs)
            .map(|row| {
                Row::new(row.iter().map(|stat| {
                    Cell::from(format!(
                        "{}{} {:>calls_width$}",
                        stat.command,
                        " ".repeat(command_width - Line::from(stat.command.as_str()).width()),
                        format_with_commas(stat.calls)
                    ))
                }))
            })
            .collect();
        let widths = vec![Constraint::Length(u16::try_from(pair_width).unwrap_or(u16::MAX)); pairs];
        let header = (0..pairs).map(|_| {
            Cell::from(format!(
                "{:<command_width$} {:>calls_width$}",
                "Command", "Calls"
            ))
        });
        frame.render_widget(
            pane.table(rows, widths, header, block).column_spacing(2),
            pane.area,
        );
        return;
    }

    draw_commandstats_table(frame, pane, stats, &visible[range], block);
}

fn draw_commandstats_table(
    frame: &mut ratatui::Frame<'_>,
    pane: &PaneCtx<'_>,
    stats: &[CommandStat],
    visible: &[&CommandStat],
    block: Block<'static>,
) {
    let app = pane.app;
    let columns = app.visible_commandstats_columns();
    let rows: Vec<Row<'_>> = visible
        .iter()
        .map(|stat| {
            Row::new(columns.iter().map(|column| {
                match column {
                    CommandstatsColumn::Command => Cell::from(stat.command.clone()),
                    CommandstatsColumn::Calls => right(format_with_commas(stat.calls)),
                    CommandstatsColumn::Usec => right(format_with_commas(stat.usec)),
                    CommandstatsColumn::UsecPerCall => right(format!("{:.2}", stat.usec_per_call)),
                    CommandstatsColumn::Metric(name) => right(
                        stat.additional_metrics
                            .get(name)
                            .map_or("-", String::as_str),
                    ),
                }
            }))
        })
        .collect();
    let widths = columns.iter().map(|column| match column {
        CommandstatsColumn::Command => Constraint::Min(20),
        CommandstatsColumn::Calls | CommandstatsColumn::Usec => Constraint::Length(14),
        CommandstatsColumn::UsecPerCall => Constraint::Length(15),
        CommandstatsColumn::Metric(name) => {
            let width = stats
                .iter()
                .filter_map(|stat| stat.additional_metrics.get(name))
                .map(|value| Line::from(value.as_str()).width())
                .chain([Line::from(name.as_str()).width()])
                .max()
                .unwrap_or(1);
            Constraint::Length(u16::try_from(width).unwrap_or(u16::MAX))
        }
    });
    let header = columns.iter().map(|column| {
        if *column == CommandstatsColumn::Command {
            Cell::from(column.header().to_string())
        } else {
            right(column.header())
        }
    });

    frame.render_widget(pane.table(rows, widths, header, block), pane.area);
}

fn draw_bigkeys(
    frame: &mut ratatui::Frame<'_>,
    pane: &PaneCtx<'_>,
    scroll: &mut Scroll,
    bigkeys: &BigkeysMetrics,
) {
    let visible = pane.app.visible_bigkeys(&bigkeys.largest_keys);
    let range = scroll.viewport(visible.len(), pane.page_len(1));

    let mut title = pane.title("Bigkeys", &range, visible.len());
    let running = bigkeys.status == BigkeysScanStatus::Running;
    if running {
        title.push_str("  scanning");
    }
    if let Some(error) = &bigkeys.last_error {
        let _ = write!(title, "  error={}", truncate_chars(error, 40, "..."));
    }
    let mut block = pane.block(title);
    if let Some(age) = bigkeys_age_title(bigkeys) {
        block = block.title(age);
    }

    let empty_message = if bigkeys.largest_keys.is_empty() {
        Some(match &bigkeys.last_error {
            _ if running => "Scanning keyspace for big keys...".to_string(),
            Some(error) => error.clone(),
            None => "No keys found".to_string(),
        })
    } else if visible.is_empty() {
        Some("No keys match the current filter".to_string())
    } else {
        None
    };
    if let Some(message) = empty_message {
        pane.message(frame, block, message);
        return;
    }

    let rows: Vec<Row<'_>> = visible[range]
        .iter()
        .map(|entry| {
            Row::new([
                Cell::from(entry.key.clone()),
                Cell::from(entry.key_type.clone()),
                right(format_optional_u64(entry.size)),
                right(format_optional_bytes(entry.memory_usage)),
            ])
        })
        .collect();
    let table = pane.table(
        rows,
        [
            Constraint::Min(26),
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(18),
        ],
        [
            Cell::from("Key"),
            Cell::from("Type"),
            right("Length"),
            right("Memory"),
        ],
        block,
    );
    frame.render_widget(table, pane.area);
}

fn draw_hotkeys(
    frame: &mut ratatui::Frame<'_>,
    pane: &PaneCtx<'_>,
    scroll: &mut Scroll,
    hotkeys: &HotkeysMetrics,
) {
    let visible = pane.app.visible_hotkeys(&hotkeys.entries);
    let range = scroll.viewport(visible.len(), pane.page_len(1));

    let metric = hotkeys.selected_metric.map_or("?", HotkeysMetric::label);
    let mut title = pane.title(&format!("Hotkeys {metric}"), &range, visible.len());
    if matches!(hotkeys.status, HotkeysStatus::Ready | HotkeysStatus::Failed) {
        title.push_str("  C/N run  R rerun  X reset");
    }
    if let Some(error) = &hotkeys.last_error {
        let _ = write!(title, "  error={}", truncate_chars(error, 40, "..."));
    }
    let mut block = pane.block(title);
    if let Some(instant) = hotkeys.last_completed {
        block = block.title(age_title(instant));
    }

    match hotkeys.status {
        HotkeysStatus::Idle => {
            pane.aligned_message(
                frame,
                block,
                "Start sampling (60 seconds)\n\n[C] CPU [N] NET".into(),
                Alignment::Center,
            );
            return;
        }
        HotkeysStatus::Running => {
            let remaining = hotkeys.remaining_seconds().unwrap_or(0);
            pane.aligned_message(
                frame,
                block,
                format!("\n\nSampling {remaining}s\nPress [X] to stop"),
                Alignment::Center,
            );
            return;
        }
        HotkeysStatus::Ready | HotkeysStatus::Failed => {}
    }

    let empty_message = if hotkeys.entries.is_empty() {
        Some(hotkeys.last_error.clone().unwrap_or_else(|| {
            "No hotkeys found\n\nPress [C] or [N] to run again, [R] to rerun the last \
             metric, or [X] to reset this pane."
                .to_string()
        }))
    } else if visible.is_empty() {
        Some("No hotkeys match the current filter".to_string())
    } else {
        None
    };
    if let Some(message) = empty_message {
        pane.message(frame, block, message);
        return;
    }

    let total_value = hotkeys.total_value.unwrap_or(0);
    let rows: Vec<Row<'_>> = visible[range]
        .iter()
        .map(|entry| {
            let share = if total_value == 0 {
                0.0
            } else {
                crate::column::u64_to_f64(entry.value) / crate::column::u64_to_f64(total_value)
                    * 100.0
            };
            Row::new([
                Cell::from(entry.key.clone()),
                right(format_with_commas(entry.value)),
                right(format!("{share:.2}%")),
            ])
        })
        .collect();
    let value_header = hotkeys
        .selected_metric
        .map_or("Value", HotkeysMetric::value_header);
    let table = pane.table(
        rows,
        [
            Constraint::Min(26),
            Constraint::Length(18),
            Constraint::Length(10),
        ],
        [Cell::from("Key"), right(value_header), right("Share")],
        block,
    );
    frame.render_widget(table, pane.area);
}

fn age_title(completed: std::time::Instant) -> Line<'static> {
    Line::from(format!("age: {}s", completed.elapsed().as_secs())).right_aligned()
}

fn bigkeys_age_title(bigkeys: &BigkeysMetrics) -> Option<Line<'static>> {
    if bigkeys.status == BigkeysScanStatus::Running {
        return None;
    }
    bigkeys.last_completed.map(age_title)
}

fn draw_detail_text(
    frame: &mut ratatui::Frame<'_>,
    pane: &PaneCtx<'_>,
    scroll: &mut Scroll,
    body: &str,
) {
    let visible_lines = pane.app.visible_detail_text_lines(pane.tab, body);
    let range = scroll.viewport(visible_lines.len(), pane.page_len(0));
    let block = pane.block(pane.title(pane.tab.title(), &range, visible_lines.len()));
    let text = if visible_lines.is_empty() {
        "No lines match the current filter".to_string()
    } else {
        visible_lines[range].join("\n")
    };
    pane.message(frame, block, text);
}

fn detail_tab_label(tab: DetailTab) -> Line<'static> {
    let shortcut = tab.shortcut().to_ascii_uppercase();
    let title = tab.title();

    if let Some((start, ch)) = title
        .char_indices()
        .find(|(_, ch)| ch.to_ascii_uppercase() == shortcut)
    {
        let end = start + ch.len_utf8();
        return Line::from(vec![
            Span::raw(title[..start].to_string()),
            Span::raw("["),
            Span::styled(
                ch.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("]{}", &title[end..])),
        ]);
    }

    Line::from(title)
}

#[cfg(test)]
fn detail_tabs_widget(app: &AppState) -> ratatui::widgets::Tabs<'static> {
    ratatui::widgets::Tabs::new(DetailTab::ALL.map(detail_tab_label))
        .style(base_style(app))
        .block(bordered(app, ""))
        .divider("│")
        .select(app.detail_tab.index())
        .highlight_style(selected_tab_style(app))
}

fn format_aligned_rows(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(label, value)| format!("{label:width$} : {value}"))
        .collect::<Vec<String>>()
        .join("\n")
}

fn detail_text_body(instance: &InstanceState, tab: DetailTab) -> String {
    match tab {
        DetailTab::Summary => summary_detail_body(instance),
        DetailTab::InfoRaw => instance
            .detail
            .raw_info
            .clone()
            .unwrap_or_else(|| "INFO not available".to_string()),
        DetailTab::Commandstats | DetailTab::Bigkeys | DetailTab::Hotkeys => String::new(),
    }
}

fn summary_detail_body(instance: &InstanceState) -> String {
    let detail = &instance.detail;
    let hits = detail.keyspace_hits.unwrap_or(0);
    let misses = detail.keyspace_misses.unwrap_or(0);
    let lookups = hits.saturating_add(misses);
    let hit_rate = if lookups == 0 {
        0.0
    } else {
        crate::column::u64_to_f64(hits) / crate::column::u64_to_f64(lookups) * 100.0
    };
    let replication_source = match (detail.master_host.as_deref(), detail.master_port) {
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
            format_optional_bytes(detail.used_memory_rss),
        ),
        ("maxmemory", format_optional_bytes(instance.maxmemory_bytes)),
        ("ops_per_sec", format_optional_u64(instance.ops_per_sec)),
        (
            "commands",
            format_optional_u64(detail.total_commands_processed),
        ),
        (
            "connected_clients",
            format_optional_u64(detail.connected_clients),
        ),
        (
            "blocked_clients",
            format_optional_u64(detail.blocked_clients),
        ),
        ("hits", format_with_commas(hits)),
        ("misses", format_with_commas(misses)),
        ("hit_rate", format!("{hit_rate:.1}%")),
        ("evicted_keys", format_optional_u64(detail.evicted_keys)),
        ("expired_keys", format_optional_u64(detail.expired_keys)),
        ("master", replication_source),
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

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), format_with_commas)
}

fn format_optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), format_bytes)
}

fn format_with_commas(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn draw_status_bar(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let [prompt_area, actions_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let prompt = if app.is_filtering {
        format!("{}: {}", app.filter_prompt_mode.label(), app.filter)
    } else if let Some(tab) = app.editing_pane() {
        format!("{} Filter: /{}", tab.title(), app.pane(tab).filter)
    } else {
        app.selected_instance()
            .and_then(|instance| {
                instance
                    .error_details
                    .as_ref()
                    .map(|details| details.summary.clone())
                    .or_else(|| instance.last_error.clone())
            })
            .unwrap_or_default()
    };
    frame.render_widget(Paragraph::new(prompt).style(base_style(app)), prompt_area);

    frame.render_widget(
        Paragraph::new(status_bar_actions(app)).style(base_style(app).add_modifier(Modifier::BOLD)),
        actions_area,
    );
}

fn status_bar_actions(app: &AppState) -> Line<'static> {
    if app.active_view == ActiveView::Detail {
        return detail_footer_actions(app);
    }

    let footer_actions = format!(
        "[H]elp  [F]ilter /  [T]ree:{}  [M]etrics  [S]ortBy  [C]olumns  [A]uth  [K]ill  [Space]Select",
        app.view_mode.footer_label()
    );
    Line::from(app.discovery_status.footer_summary().map_or_else(
        || footer_actions.clone(),
        |summary| format!("{summary}  |  {footer_actions}"),
    ))
}

fn detail_footer_actions(app: &AppState) -> Line<'static> {
    let mut spans = Vec::new();
    for tab in DetailTab::ALL {
        if !spans.is_empty() {
            spans.push(Span::raw("  │  "));
        }
        let style = if tab == app.detail_tab {
            selected_tab_style(app)
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
    if app.detail_tab == DetailTab::Commandstats {
        spans.push(Span::raw("  [P]Compact  [F7]Columns  [R]eset"));
    }
    spans.push(Span::raw("  [H]elp"));
    Line::from(spans)
}

const fn overview_help_bindings() -> &'static [(&'static str, &'static str)] {
    &[
        ("q", "Quit, or close the active overlay"),
        ("Ctrl+C", "Quit immediately"),
        ("H / F1", "Open help for the current view"),
        ("t / F5", "Cycle Tree, Flat, and Primary view in overview"),
        ("m / M", "Show or hide the top activity metrics panel"),
        ("s / F6", "Choose sort column in overview"),
        ("c / F7 / v", "Toggle and reorder overview columns"),
        (
            "a / F8",
            "Enter credentials for selected servers (or the focused server)",
        ),
        (
            "K / F9",
            "Stop selected servers (or the focused server); confirm batch stops",
        ),
        (
            "Space",
            "Select server for Commandstats, activity totals and batch actions (none = all activity)",
        ),
        (
            "Hold Space",
            "700 ms: primary + replicas; 1.5 s: cluster; same select/deselect direction",
        ),
        ("N + motion", "Move N steps with h/j/k/l or arrow keys"),
        (
            "NSpace",
            "Select N servers downward, including the focused row",
        ),
        (
            "Nj/k Space",
            "Space within 500 ms selects N rows from the original focus",
        ),
        ("PgUp / PgDn", "Move or scroll a page at a time"),
        (
            "g / G, Home / End",
            "Jump to the first or last row (NG jumps to row N)",
        ),
        ("Case", "K (kill/Hotkeys) requires uppercase"),
        ("f or /", "Edit the overview filter (keeps existing text)"),
        (
            "Esc",
            "Close overlay/filter/detail/help; clear selection, then quit",
        ),
        ("Enter", "Open detail view for the focused server"),
        ("Up/Down/j/k", "Move focus in overview"),
        (
            "Tab/Right/l",
            "Next detail panel (Shift+Tab/Left/h goes back)",
        ),
        (
            "S / I / C / B / K",
            "Open Summary / Info Raw / Commandstats / Bigkeys / Hotkeys; C is pane-specific in Commandstats/Hotkeys",
        ),
        ("Enter / Esc", "Finish filter editing"),
        ("?", "Toggle help overlay"),
        ("r / R", "Refresh now"),
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
            "o / O",
            "Toggle host rendering (auto hide when all hosts are the same)",
        ),
        ("/", "Start filter input in overview"),
        ("Backspace", "Delete filter character while editing"),
    ]
}

/// A rectangle of the given percentage of `area`, centered within it.
fn centered_percent(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    centered(
        area,
        area.width.saturating_mul(width_pct) / 100,
        area.height.saturating_mul(height_pct) / 100,
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn help_bindings(app: &AppState) -> Vec<(&'static str, &'static str)> {
    let view = app.active_view;
    if view != ActiveView::Detail {
        return overview_help_bindings().to_vec();
    }
    let mut bindings = vec![
        ("Esc", "Return to overview"),
        ("Tab", "Next detail panel"),
        ("Shift+Tab", "Previous detail panel"),
        ("/", "Edit this panel's filter"),
    ];
    bindings.extend_from_slice(match app.detail_tab {
        DetailTab::Commandstats => &[
            ("c / C / v / V / F7", "Choose columns"),
            ("p / P", "Toggle compact command/calls layout"),
            ("r / R", "Reset statistics (asks for confirmation)"),
        ][..],
        DetailTab::Bigkeys => &[("r / R", "Rerun the Bigkeys scan")][..],
        DetailTab::Hotkeys => &[
            ("C / N", "Start CPU / NET hotkeys sampling"),
            ("X", "Stop sampling, or reset this panel when idle"),
            ("r / R", "Rerun sampling with the last selected metric"),
        ][..],
        DetailTab::Summary | DetailTab::InfoRaw => &[("r / R", "Refresh now")][..],
    });
    bindings
}

fn scroll_popup(app: &mut AppState, key: KeyEvent) {
    if key.kind == KeyEventKind::Release || has_command_modifier(key) {
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.popup_scroll.scroll_by(-1),
        KeyCode::Down | KeyCode::Char('j') => app.popup_scroll.scroll_by(1),
        KeyCode::PageUp => app.popup_scroll.page_by(-1),
        KeyCode::PageDown => app.popup_scroll.page_by(1),
        KeyCode::Home | KeyCode::Char('g') => app.popup_scroll.to_start(),
        KeyCode::End | KeyCode::Char('G') => app.popup_scroll.to_end(),
        _ => {}
    }
}

fn handle_help_key(app: &mut AppState, key: KeyEvent) {
    if key.kind == KeyEventKind::Press
        && matches!(
            key.code,
            KeyCode::Char('H' | '?' | 'q') | KeyCode::F(1) | KeyCode::Esc
        )
    {
        app.show_help = false;
    } else {
        scroll_popup(app, key);
    }
}

fn draw_help_overlay(frame: &mut ratatui::Frame<'_>, app: &mut AppState, area: Rect) {
    let view = app.active_view;
    let context = if view == ActiveView::Detail {
        app.detail_tab.title()
    } else {
        "Overview"
    };
    let title = if view == ActiveView::Detail {
        format!("{context} Help — Esc/q closes")
    } else {
        format!("{context} Help — ↑↓/PgUp/PgDn scroll; Esc/q closes")
    };
    let text = help_bindings(app)
        .iter()
        .map(|(keys, description)| format!("{keys}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    draw_text_popup(frame, app, area, title, text);
}

fn draw_text_popup(
    frame: &mut ratatui::Frame<'_>,
    app: &mut AppState,
    area: Rect,
    title: String,
    text: String,
) {
    let bounds = centered_percent(area, 90, 85);
    let text = Text::from(text);
    let width = u16::try_from(text.width().max(Line::from(title.as_str()).width()))
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(bounds.width);
    let paragraph = Paragraph::new(text)
        .style(base_style(app))
        .wrap(Wrap { trim: false });
    let lines = paragraph.line_count(width.saturating_sub(2));
    let height = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(bounds.height);
    let popup = centered(area, width, height);
    let range = app
        .popup_scroll
        .viewport(lines, usize::from(popup.height.saturating_sub(2)));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        paragraph
            .scroll((u16::try_from(range.start).unwrap_or(u16::MAX), 0))
            .block(bordered(app, title)),
        popup,
    );
}

fn draw_reset_stats_popup(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut AppState) {
    let (title, text) = if app.overview_modal == OverviewModal::ResetStatsConfirmation {
        (
            "Confirm statistics reset",
            format!(
                "Send CONFIG RESETSTAT to {} servers? y/N",
                app.reset_stats_targets.len(),
            ),
        )
    } else {
        (
            "Statistics reset results",
            format!(
                "{}\n\nEnter/Esc/q closes; ↑↓/PgUp/PgDn scroll.",
                app.reset_stats_result
            ),
        )
    };
    draw_text_popup(frame, app, area, title.to_string(), text);
}

/// Single-column selectable list used by every picker dialog.
fn draw_picker(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    popup: Rect,
    title: String,
    items: Vec<String>,
    selected: usize,
    highlight_symbol: &'static str,
) {
    let table = Table::new(
        items.into_iter().map(|item| Row::new([item])),
        [Constraint::Percentage(100)],
    )
    .block(bordered(app, title))
    .style(base_style(app))
    .row_highlight_style(highlight_style(app))
    .highlight_symbol(highlight_symbol);
    let mut state = TableState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, popup, &mut state);
}

fn draw_sort_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let items = app
        .sortable_columns()
        .iter()
        .map(|key| {
            let label = app.column_label(key);
            if *key == app.sort_by {
                format!("{label} ({})", sort_direction_symbol(app.sort_direction))
            } else {
                label
            }
        })
        .collect();
    draw_picker(
        frame,
        app,
        centered_percent(area, 45, 55),
        "Sort By (Enter select, Esc cancel)".into(),
        items,
        app.sort_picker_index,
        "> ",
    );
}

fn draw_column_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let items = app
        .column_picker_entries()
        .iter()
        .map(|entry| {
            let checked = if entry.visible { "[x]" } else { "[ ]" };
            format!("{checked} {}{}", entry.label, entry.suffix)
        })
        .collect();
    let (title, symbol) = if app.column_picker_reorder_mode {
        (
            "Columns (Shift+Up/Down move, Enter/Space toggle, Esc close)",
            "↕ ",
        )
    } else {
        (
            "Columns (Enter/Space toggle, Shift+Up/Down move, Esc close)",
            "> ",
        )
    };
    draw_picker(
        frame,
        app,
        centered_percent(area, 55, 60),
        title.into(),
        items,
        app.column_picker_index,
        symbol,
    );
}

fn draw_kill_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let signal_supported = selected_signal_supported(app);
    let items = KillAction::ALL
        .iter()
        .map(|action| {
            let suffix = if action.is_signal() && !signal_supported {
                " (needs local process_id)"
            } else {
                ""
            };
            format!("{}{suffix}", action.label())
        })
        .collect();
    let targets = action_targets_label(app, &app.kill_target_keys);
    draw_picker(
        frame,
        app,
        centered_percent(area, 45, 60),
        format!("Kill {targets} (Enter select, Esc cancel)"),
        items,
        app.kill_picker_index,
        "> ",
    );
}

fn draw_auth_form(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let Some(form) = &app.auth_form else {
        return;
    };
    let width = (area.width.saturating_mul(60) / 100).clamp(36, 72);
    let popup = centered(area, width, 8);
    let [fields_area, hint_area] =
        Layout::vertical([Constraint::Length(5), Constraint::Length(3)]).areas(popup);
    let target = action_targets_label(app, &form.target_keys);
    let password_mask = "•".repeat(form.password.chars().count());
    let rows = [
        Row::new([Cell::from("Username"), Cell::from(form.username.clone())]),
        Row::new([Cell::from("Password"), Cell::from(password_mask)]),
    ];
    let table = Table::new(rows, [Constraint::Length(12), Constraint::Min(1)])
        .block(bordered(app, format!("Authenticate {target}")))
        .style(base_style(app))
        .row_highlight_style(highlight_style(app))
        .highlight_symbol("> ");
    let selected = match form.active_field {
        AuthField::Username => 0,
        AuthField::Password => 1,
    };
    let mut state = TableState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(table, fields_area, &mut state);
    frame.render_widget(
        Paragraph::new(
            "Username defaults to default. Tab switches fields; Enter connects; Esc cancels.",
        )
        .style(base_style(app))
        .wrap(Wrap { trim: true }),
        hint_area,
    );
}

fn action_targets_label(app: &AppState, keys: &[String]) -> String {
    if let [key] = keys {
        app.instances.get(key).map_or_else(
            || key.clone(),
            |instance| instance.display_name().to_string(),
        )
    } else {
        format!("{} servers", keys.len())
    }
}

fn selected_signal_supported(app: &AppState) -> bool {
    let keys = if app.kill_target_keys.is_empty() {
        app.action_target_keys()
    } else {
        app.kill_target_keys.clone()
    };
    !keys.is_empty()
        && keys.iter().all(|key| {
            app.instances.get(key).is_some_and(|instance| {
                instance.detail.process_id.is_some() && is_local_addr(&instance.addr)
            })
        })
}

fn draw_kill_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppState) {
    let Some(action) = app.selected_kill_action() else {
        return;
    };
    let popup = centered(area, 64, 6);
    let prompt = format!(
        "Stop {} servers with {}?\n\nEnter confirms; Esc or q cancels.",
        app.kill_target_keys.len(),
        action.label(),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(prompt)
            .block(Block::default().borders(Borders::ALL).title("Confirm stop"))
            .style(base_style(app))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn bordered(app: &AppState, title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(base_style(app))
}

fn base_style(app: &AppState) -> Style {
    Style::default()
        .fg(foreground_color(app))
        .bg(background_color(app))
}

fn highlight_style(app: &AppState) -> Style {
    Style::default()
        .fg(carat_color(app))
        .bg(background_color(app))
        .add_modifier(Modifier::BOLD)
}

fn selected_tab_style(app: &AppState) -> Style {
    Style::default()
        .fg(background_color(app))
        .bg(carat_color(app))
        .add_modifier(Modifier::BOLD)
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
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableFocusChange,
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
        DisableFocusChange,
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
        Feeds, background_color, bigkeys_age_title, carat_color, compute_column_widths,
        detail_tabs_widget, draw, draw_status_bar, edit_text_input, format_aligned_rows,
        format_with_commas, handle_column_picker_key, handle_commandstats_shortcut,
        handle_overlay_quit_key, handle_overview_shortcut, handle_primary_view_quit_key,
        is_force_quit_key, overview_help_bindings, selected_signal_supported,
    };
    use crate::app::{ActiveView, AppState, DetailTab, OverviewModal};
    use crate::column::{Align, CellCtx, Column, SortKey, WidthHint};
    use crate::config::default_settings;
    use crate::model::{
        BigkeyEntry, BigkeysScanStatus, CommandStat, ErrorDetails, InstanceState, SortMode, Status,
        ViewMode,
    };
    use crate::overview::{cluster_color_for_token, fit_cell_text, render_plain_text};
    use crate::poller::PollerRequest;
    use crate::registry::ColumnRegistry;
    use tokio::sync::mpsc;

    /// Feeds whose poller side is the returned receiver, for asserting requests.
    fn test_feeds() -> (Feeds, mpsc::Receiver<PollerRequest>) {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (_, updates_rx) = mpsc::channel(1);
        let (_, discovery_rx) = mpsc::channel(1);
        (
            Feeds {
                updates_rx,
                discovery_rx,
                request_tx,
            },
            request_rx,
        )
    }

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

    fn app_with_selected_servers() -> AppState {
        let mut app = AppState::new(default_settings(), test_registry());
        for port in [6379, 6380, 6381] {
            let addr = format!("127.0.0.1:{port}");
            app.apply_update(InstanceState::new(addr.clone(), addr));
        }
        for _ in 0..2 {
            assert!(handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            ));
            app.move_selection(1);
        }
        app
    }

    #[test]
    fn activity_panel_renders_graphs_and_tracks_keyboard_selection() {
        let mut app = AppState::new(default_settings(), test_registry());
        let now = std::time::Instant::now();
        for port in [6379, 6380] {
            let addr = format!("127.0.0.1:{port}");
            let mut state = InstanceState::new(addr.clone(), addr);
            state.status = Status::Ok;
            state.last_updated = Some(now);
            state.ops_per_sec = Some(100);
            state.used_memory_bytes = Some(1024);
            state.detail.connected_clients = Some(2);
            for (key, value) in [
                ("used_cpu_sys", "0"),
                ("used_cpu_user", "1"),
                ("instantaneous_input_kbps", "1"),
                ("instantaneous_output_kbps", "2"),
            ] {
                state.info.insert(key.into(), value.into());
            }
            app.apply_update(state.clone());
            state.last_updated = Some(now + std::time::Duration::from_secs(1));
            state.info.insert("used_cpu_user".into(), "1.25".into());
            app.apply_update(state);
        }
        let at = now + std::time::Duration::from_secs(1);
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines[0].contains("Activity | all 2 | 2/2 live"));
        assert!(lines[..6].iter().all(|line| !line.contains("127.0.0.1:")));
        assert!(lines[1].contains("Memory 2 KiB  Clients 4"));
        assert!(lines[2].contains("CPU 50.0%"));
        assert!(lines[3].contains("Ops 200/s"));
        assert!(lines[4].contains("Net ↓2 KiB/s ↑4 KiB/s"));
        assert!(lines[2..5].iter().all(|line| !line.contains('█')));
        assert!(lines[2].contains("200% max"));
        assert!(lines[3].contains("2,000/s max"));
        assert!(lines[4].contains("2 MiB/s max"));
        assert_eq!(app.overview_page_len, 13);

        // Resizing changes the visible history, not its vertical scale.
        let mut narrow = Terminal::new(TestBackend::new(80, 24)).unwrap();
        narrow.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(narrow.backend().buffer());
        assert!(lines[2].contains("200% max"));
        assert!(lines[3].contains("2,000/s max"));
        assert!(lines[4].contains("2 MiB/s max"));

        let mut navigation = super::navigation::Navigation::default();
        assert!(navigation.handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            at
        ));
        assert!(handle_overview_shortcut(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)
        ));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines[0].contains("Activity | selected 1 | 1/1 live"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("6380") && line.chars().nth(1) == Some('▶'))
        );
        assert!(lines[3].contains("Ops 100/s"));
        assert!(lines[2].contains("100% max"));
        assert!(lines[3].contains("1,000/s max"));
        assert!(lines[4].contains("1 MiB/s max"));
        assert_eq!(app.activity.history.len(), 1);
    }

    #[test]
    fn configured_activity_visibility_can_be_toggled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtop.toml");
        for (config, visible) in [
            ("", true),
            ("show_activity = true", true),
            ("show_activity = false", false),
        ] {
            std::fs::write(&path, format!("[global]\n{config}\n")).unwrap();
            let loaded = crate::config::load_config(Some(&path), false).unwrap();
            let settings = crate::config::apply_overrides(default_settings(), &loaded.overrides);
            let mut app = AppState::new(settings, test_registry());
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            assert_eq!(app.show_activity, visible);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert_eq!(
                buffer_lines(terminal.backend().buffer())[0].contains("Activity |"),
                visible
            );
            for (key, expected) in [('m', !visible), ('M', visible)] {
                assert!(handle_overview_shortcut(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
                ));
                assert_eq!(app.show_activity, expected);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                assert_eq!(
                    buffer_lines(terminal.backend().buffer())[0].contains("Activity |"),
                    app.show_activity
                );
            }
            assert_eq!(app.show_activity, visible);
        }
    }

    #[test]
    fn metrics_toggle_reclaims_table_space_and_restores_panel() {
        for (height, panel_height) in [(24, 6), (12, 4), (10, 0)] {
            let mut app = app_with_selected_servers();
            let selected = app.selected_key();
            let mut terminal = Terminal::new(TestBackend::new(120, height)).unwrap();
            assert!(app.show_activity);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let original_page_len = app.overview_page_len;
            let history_len = app.activity.history.len();

            assert!(handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            ));
            assert!(!app.show_activity);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            assert!(!lines.iter().any(|line| line.contains("Activity |")));
            assert!(lines.last().unwrap().contains("[M]etrics"));
            assert_eq!(app.overview_page_len, original_page_len + panel_height);
            assert_eq!(app.selected_key(), selected);
            assert_eq!(app.selected_server_count(), 2);
            assert!(app.activity.history.len() >= history_len);

            // Sampling continues while hidden, preserving the existing history.
            app.sample_activity(std::time::Instant::now() + app.settings.refresh_interval);
            assert!(app.activity.history.len() > history_len);
            assert!(handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT),
            ));
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(app.show_activity);
            assert_eq!(app.overview_page_len, original_page_len);
            let lines = buffer_lines(terminal.backend().buffer());
            assert_eq!(lines[0].contains("Activity |"), panel_height > 0);
        }
        assert!(
            overview_help_bindings()
                .iter()
                .any(|(keys, _)| *keys == "m / M")
        );
    }

    #[test]
    fn metrics_toggle_ignores_help_overlay_modifiers_and_non_press_events() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.show_help = true;
        assert!(!handle_overview_shortcut(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        ));
        app.show_help = false;
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Char('m'), KeyModifiers::NONE, kind),
            ));
        }
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char('m'), modifiers),
            ));
        }
        assert!(app.show_activity);
    }

    #[test]
    fn activity_panel_handles_small_empty_and_scrolled_views() {
        let mut app = AppState::new(default_settings(), test_registry());
        for (width, height) in [(120, 24), (80, 12), (20, 10), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if height >= 12 {
                let lines = buffer_lines(terminal.backend().buffer());
                assert!(lines[1].contains("Memory -"));
                if height == 12 {
                    assert!(lines[2].contains("Ops -/s"));
                }
            }
        }
        for port in 6379..6399 {
            let addr = format!("127.0.0.1:{port}");
            app.apply_update(InstanceState::new(addr.clone(), addr));
        }
        app.selected_index = 19;
        app.sample_activity(std::time::Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("6398") && line.chars().nth(1) == Some('>'))
        );
        assert!(lines[0].contains("0/20 live | partial data"));
    }

    #[test]
    fn overview_shows_marked_servers_separately_from_focus() {
        let mut app = app_with_selected_servers();
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("2 selected")));
        assert!(lines.iter().any(|line| line.contains("[Space]Select")));
        for port in ["6379", "6380"] {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(port) && line.chars().nth(1) == Some('●'))
            );
        }
        assert!(
            lines
                .iter()
                .any(|line| line.contains("6381") && line.chars().nth(1) == Some('>'))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("[x]") || line.contains("[ ]"))
        );

        app.move_selection(-2);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        for (port, marker) in [("6379", '▶'), ("6380", '●'), ("6381", ' ')] {
            let row = lines.iter().rposition(|line| line.contains(port)).unwrap();
            assert_eq!(lines[row].chars().nth(1), Some(marker));
            if marker == '▶' {
                let cell = &buffer[(1, u16::try_from(row).unwrap())];
                assert_eq!(cell.fg, carat_color(&app));
                assert!(cell.modifier.contains(Modifier::BOLD));
            }
        }

        app.toggle_server_selection();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .iter()
                .any(|line| { line.contains("6379") && line.chars().nth(1) == Some('>') })
        );
        app.toggle_server_selection();
        app.open_auth_form();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .join("\n")
                .contains("Authenticate 2 servers")
        );
    }

    #[test]
    fn batch_stop_requires_a_second_press_and_preserves_targets_and_action() {
        let mut app = app_with_selected_servers();
        let (tx, mut rx) = test_feeds();
        app.open_kill_picker();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(super::navigation::Navigation::default().handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            std::time::Instant::now(),
        ));
        super::handle_kill_key(&mut app, enter, &tx).unwrap();
        assert_eq!(app.overview_modal, OverviewModal::KillConfirmation);
        assert!(rx.try_recv().is_err());

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .join("\n")
                .contains("Stop 2 servers with SHUTDOWN NOSAVE?")
        );

        super::handle_kill_key(
            &mut app,
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
            &tx,
        )
        .unwrap();
        super::handle_kill_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        assert!(rx.try_recv().is_err());
        app.sort_direction = crate::model::SortDirection::Desc;
        app.remove_instance("127.0.0.1:6379");
        super::handle_kill_key(&mut app, enter, &tx).unwrap();
        let crate::poller::PollerRequest::KillTargets { keys, action } = rx.try_recv().unwrap()
        else {
            panic!("expected batch stop request");
        };
        assert_eq!(keys, ["127.0.0.1:6379", "127.0.0.1:6380"]);
        assert_eq!(action, crate::model::KillAction::ShutdownNosave);
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn batch_stop_cancellation_never_queues_a_request() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut app = app_with_selected_servers();
            let (tx, mut rx) = test_feeds();
            app.open_kill_picker();
            super::handle_kill_key(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &tx,
            )
            .unwrap();
            assert!(handle_overlay_quit_key(
                &mut app,
                KeyEvent::new(code, KeyModifiers::NONE)
            ));
            assert!(rx.try_recv().is_err());
            assert_eq!(app.overview_modal, OverviewModal::None);
            assert_eq!(app.kill_target_keys, Vec::<String>::new());
            assert_eq!(app.selected_server_count(), 2);
            assert!(!app.should_quit);
        }
    }

    #[test]
    fn space_only_selects_servers_in_overview_without_an_input_or_modal() {
        let mut app = app_with_selected_servers();
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        app.is_filtering = true;
        assert!(!handle_overview_shortcut(&mut app, space));
        app.is_filtering = false;
        app.active_view = ActiveView::Detail;
        assert!(!handle_overview_shortcut(&mut app, space));
        app.active_view = ActiveView::Overview;
        for modal in [
            OverviewModal::AuthForm,
            OverviewModal::ColumnPicker,
            OverviewModal::KillConfirmation,
        ] {
            app.overview_modal = modal;
            assert!(!handle_overview_shortcut(&mut app, space));
        }
        assert_eq!(app.selected_server_count(), 2);
    }

    #[test]
    fn overview_colors_status_by_severity_and_shows_its_settings() {
        let mut app = AppState::new(default_settings(), test_registry());
        let mut healthy = InstanceState::new("ok".into(), "127.0.0.1:6379".into());
        healthy.status = Status::Ok;
        let mut loading = InstanceState::new("loading".into(), "127.0.0.1:6380".into());
        loading.status = Status::Loading;
        let down = InstanceState::new("down".into(), "127.0.0.1:6381".into());
        for state in [healthy, loading, down] {
            app.apply_update(state);
        }

        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("refresh=") && line.contains("view=Tree"))
        );

        let theme = app.settings.ui_theme;
        for (status, expected) in [
            ("OK", Color::Reset),
            ("LOADING", theme.warning.to_ratatui_color()),
            ("DOWN", theme.critical.to_ratatui_color()),
        ] {
            let row = lines
                .iter()
                .position(|line| line.contains(&format!(" {status} ")))
                .expect("status rendered");
            let x = char_column(&lines[row], status);
            let cell = &buffer[(u16::try_from(x).unwrap(), u16::try_from(row).unwrap())];
            if expected == Color::Reset {
                assert_eq!(cell.fg, app.settings.ui_theme.foreground.to_ratatui_color());
            } else {
                assert_eq!(cell.fg, expected, "{status}");
            }
        }
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
    fn help_is_contextual_scrollable_and_blocks_underlying_commands() {
        let mut app = app_with_selected_servers();
        let (feeds, mut requests) = test_feeds();
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        for view in [ActiveView::Overview, ActiveView::Detail] {
            app.active_view = view;
            for tab in DetailTab::ALL {
                app.detail_tab = tab;
                let bindings = super::help_bindings(&app);
                assert_eq!(
                    bindings.iter().any(|(keys, _)| *keys == "Space"),
                    view == ActiveView::Overview
                );
                assert_eq!(
                    bindings.iter().any(|(keys, _)| *keys == "X"),
                    view == ActiveView::Detail && tab == DetailTab::Hotkeys
                );
                assert_eq!(
                    bindings
                        .iter()
                        .any(|(_, text)| text.contains("Reset statistics")),
                    view == ActiveView::Detail && tab == DetailTab::Commandstats
                );
                for code in [KeyCode::Char('H'), KeyCode::F(1), KeyCode::Char('?')] {
                    super::handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE), &feeds)
                        .unwrap();
                    assert!(app.show_help);
                    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                    let context = if view == ActiveView::Overview {
                        "Overview"
                    } else {
                        tab.title()
                    };
                    assert!(
                        buffer_lines(terminal.backend().buffer())
                            .join("\n")
                            .contains(&format!("{context} Help"))
                    );
                    for blocked in ['R', '/', ' ', 'C', 'p'] {
                        super::handle_key(
                            &mut app,
                            KeyEvent::new(KeyCode::Char(blocked), KeyModifiers::NONE),
                            &feeds,
                        )
                        .unwrap();
                    }
                    assert!(requests.try_recv().is_err());
                    assert_eq!(app.overview_modal, OverviewModal::None);
                    assert_eq!(app.detail_tab, tab);
                    assert!(!app.is_filtering);
                    assert!(app.editing_pane().is_none());
                    super::handle_key(
                        &mut app,
                        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                        &feeds,
                    )
                    .unwrap();
                    assert!(app.popup_scroll.offset() > 0);
                    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                    super::handle_key(
                        &mut app,
                        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                        &feeds,
                    )
                    .unwrap();
                    assert!(!app.show_help);
                    assert_eq!(app.active_view, view);
                }
            }
        }
    }

    #[test]
    fn detail_help_is_compact_and_omits_global_navigation() {
        let mut app = app_with_selected_servers();
        app.active_view = ActiveView::Detail;
        for tab in DetailTab::ALL {
            app.detail_tab = tab;
            let bindings = super::help_bindings(&app);
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| super::draw_help_overlay(frame, &mut app, frame.area()))
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            assert_eq!(
                lines.iter().filter(|line| !line.trim().is_empty()).count(),
                bindings.len() + 2
            );
            let rendered = lines.join("\n");
            assert!(rendered.contains("Tab: Next detail panel"));
            assert!(rendered.contains("/: Edit this panel's filter"));
            for global in ["Ctrl+C", "Up/Down", "PgUp", "motion", "Home", "arrow"] {
                assert!(!rendered.contains(global), "{tab:?}: {global}");
            }
            assert_eq!(app.popup_scroll.offset(), 0);
        }
    }

    #[test]
    fn commandstats_reset_confirms_captured_nodes_and_can_be_cancelled() {
        let mut app = app_with_selected_servers();
        let expected = app.action_target_keys();
        app.filter = "6381".to_string(); // Both selected nodes are hidden.
        app.clamp_selection();
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        let (feeds, mut requests) = test_feeds();
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for cancel in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Enter,
        ] {
            super::handle_key(&mut app, press(KeyCode::Char('R')), &feeds).unwrap();
            assert_eq!(app.overview_modal, OverviewModal::ResetStatsConfirmation);
            assert_eq!(app.reset_stats_targets, expected);
            assert!(requests.try_recv().is_err());
            super::handle_key(&mut app, press(cancel), &feeds).unwrap();
            assert_eq!(app.overview_modal, OverviewModal::None);
            assert_eq!(app.reset_stats_targets, Vec::<String>::new());
            assert!(requests.try_recv().is_err());
        }
        super::handle_key(&mut app, press(KeyCode::Char('r')), &feeds).unwrap();
        app.clear_server_selection(); // Confirmation must retain its original targets.
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            super::handle_key(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, kind),
                &feeds,
            )
            .unwrap();
            assert!(requests.try_recv().is_err());
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| super::draw_reset_stats_popup(frame, frame.area(), &mut app))
            .unwrap();
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Send CONFIG RESETSTAT to 2 servers? y/N"));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            3
        );
        for key in &expected {
            assert!(!rendered.contains(key));
        }
        super::handle_key(&mut app, press(KeyCode::Char('y')), &feeds).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::ResetStats { keys } if keys == expected)
        );
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert!(app.reset_stats_result.is_empty());
        assert!(requests.try_recv().is_err());
        super::handle_key(&mut app, press(KeyCode::Char('R')), &feeds).unwrap();
        assert_eq!(app.reset_stats_targets, ["127.0.0.1:6381"]);
        super::handle_key(&mut app, press(KeyCode::Char('Y')), &feeds).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), PollerRequest::ResetStats { keys } if keys == ["127.0.0.1:6381"])
        );
    }

    #[test]
    fn commandstats_reset_respects_input_context_and_queue_errors() {
        let mut app = app_with_selected_servers();
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        let (feeds, mut requests) = test_feeds();
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE);
        app.start_active_detail_filter_input(false);
        super::handle_key(&mut app, key, &feeds).unwrap();
        assert_eq!(app.active_pane().filter, "R");
        app.active_pane_mut().is_filtering = false;
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(!handle_commandstats_shortcut(
                &mut app,
                KeyEvent::new_with_kind(key.code, key.modifiers, kind)
            ));
        }
        assert!(!handle_commandstats_shortcut(
            &mut app,
            KeyEvent::new(key.code, KeyModifiers::CONTROL)
        ));
        assert!(requests.try_recv().is_err());
        super::handle_key(&mut app, key, &feeds).unwrap();
        super::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &feeds,
        )
        .unwrap();
        assert!(requests.try_recv().is_err());
        feeds.send(PollerRequest::RefreshAll); // Fill the request queue.
        assert!(
            super::handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &feeds
            )
            .is_err()
        );
        assert_eq!(app.overview_modal, OverviewModal::ResetStatsConfirmation);
        assert_eq!(app.reset_stats_targets.len(), 2);
    }

    #[test]
    fn successful_reset_does_not_show_results() {
        let mut app = app_with_selected_servers();
        let (mut feeds, _) = test_feeds();
        let (updates, receiver) = mpsc::channel(1);
        feeds.updates_rx = receiver;
        updates
            .try_send(crate::poller::PollerUpdate::ResetStatsComplete {
                results: vec![
                    ("node-a".to_string(), Ok(())),
                    ("node-b".to_string(), Ok(())),
                ],
            })
            .unwrap();
        feeds.drain_into(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert!(app.reset_stats_result.is_empty());
    }

    #[test]
    fn reset_results_wait_for_active_input_and_show_each_node_outcome() {
        let mut app = app_with_selected_servers();
        let (mut feeds, _) = test_feeds();
        let (updates, receiver) = mpsc::channel(1);
        feeds.updates_rx = receiver;
        app.open_auth_form();
        updates
            .try_send(crate::poller::PollerUpdate::ResetStatsComplete {
                results: vec![
                    ("node-a".to_string(), Ok(())),
                    ("node-b".to_string(), Err("NOPERM".to_string())),
                ],
            })
            .unwrap();
        feeds.drain_into(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.is_auth_form_open());
        app.close_auth_form();
        app.is_filtering = true;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.overview_modal, OverviewModal::None);
        app.is_filtering = false;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.overview_modal, OverviewModal::ResetStatsResult);
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("node-a: statistics reset"));
        assert!(rendered.contains("node-b: reset failed: NOPERM"));
        app.close_overview_modal();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn help_bindings_include_help_popup_shortcut() {
        assert!(
            overview_help_bindings()
                .iter()
                .any(|(keys, _)| *keys == "H / F1")
        );
    }

    #[test]
    fn help_bindings_include_hotkeys_stop_or_reset_shortcut() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Hotkeys;
        assert!(
            super::help_bindings(&app)
                .iter()
                .any(|(keys, _)| *keys == "X")
        );
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
            "K / F9",
            "f or /",
        ] {
            assert!(
                overview_help_bindings()
                    .iter()
                    .any(|(keys, _)| *keys == binding)
            );
        }
    }

    #[test]
    fn help_bindings_describe_three_view_cycle() {
        assert!(
            overview_help_bindings()
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
                    KeyCode::Char('V'),
                    KeyCode::F(7),
                ],
                OverviewModal::ColumnPicker,
            ),
            (
                vec![KeyCode::Char('a'), KeyCode::Char('A'), KeyCode::F(8)],
                OverviewModal::AuthForm,
            ),
            (
                vec![KeyCode::Char('K'), KeyCode::F(9)],
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
            'a', 'A', 'f', 'F', '/', 't', 'T', 's', 'S', 'c', 'C', 'k', 'K', 'm', 'M',
        ];
        app.active_view = ActiveView::Detail;
        for ch in shortcuts {
            assert!(!handle_overview_shortcut(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
            ));
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
            KeyCode::Char('K'),
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
    fn detail_tab_shortcuts_reserve_lowercase_motion_keys() {
        for (ch, expected) in [
            ('s', Some(DetailTab::Summary)),
            ('L', None),
            ('i', Some(DetailTab::InfoRaw)),
            ('C', Some(DetailTab::Commandstats)),
            ('b', Some(DetailTab::Bigkeys)),
            ('K', Some(DetailTab::Hotkeys)),
            ('k', None),
            ('l', None),
            ('x', None),
        ] {
            assert_eq!(DetailTab::from_shortcut(ch), expected, "{ch}");
        }
    }

    #[test]
    fn text_input_ignores_key_releases() {
        let mut text = String::new();
        let mut editing = true;
        for kind in [KeyEventKind::Press, KeyEventKind::Release] {
            edit_text_input(
                &mut text,
                &mut editing,
                KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, kind),
            );
        }
        assert_eq!(text, "a");
        edit_text_input(
            &mut text,
            &mut editing,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(!editing);
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
    fn esc_clears_all_selections_before_a_second_press_exits() {
        for count in [1, 2] {
            let mut app = app_with_selected_servers();
            if count == 1 {
                app.move_selection(-1);
                app.toggle_server_selection();
            }
            assert_eq!(app.selected_server_count(), count);
            // Hidden selections must be cleared too.
            app.filter = "6381".to_string();
            app.clamp_selection();
            let focused = app.selected_key();
            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            assert!(handle_primary_view_quit_key(&mut app, esc));
            assert_eq!(app.selected_server_count(), 0);
            assert_eq!(app.selected_key(), focused);
            assert!(!app.should_quit);

            assert!(handle_primary_view_quit_key(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Repeat),
            ));
            assert!(!app.should_quit);
            assert!(handle_primary_view_quit_key(&mut app, esc));
            assert!(app.should_quit);
        }
    }

    #[test]
    fn esc_preserves_selection_while_leaving_other_input_contexts() {
        let mut app = app_with_selected_servers();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.open_column_picker();
        assert!(!handle_primary_view_quit_key(&mut app, esc));
        assert!(handle_overlay_quit_key(&mut app, esc));
        assert_eq!(app.selected_server_count(), 2);
        app.is_filtering = true;
        assert!(!handle_primary_view_quit_key(&mut app, esc));
        app.is_filtering = false;
        app.active_view = ActiveView::Detail;
        assert!(!handle_primary_view_quit_key(&mut app, esc));
        assert_eq!(app.selected_server_count(), 2);
        assert!(!app.should_quit);
    }

    #[test]
    fn q_still_exits_with_servers_selected() {
        let mut app = app_with_selected_servers();
        assert!(handle_primary_view_quit_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ));
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

        fn render_cell(&self, _ctx: &CellCtx<'_>) -> String {
            String::new()
        }

        fn sort_key(&self, _ctx: &CellCtx<'_>) -> SortKey {
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
            .rposition(|line| line.contains("6379"))
            .expect("cluster row rendered");
        let width = usize::from(buffer.area.width);
        let row_start = row * width;
        let row_end = row_start + width;
        let gutter_cell = buffer.content()[row_start..row_end]
            .iter()
            .find(|cell| {
                cell.symbol() == "│" && cell.fg == cluster_color_for_token("2").to_ratatui_color()
            })
            .expect("cluster gutter cell rendered with logical-cluster color");

        assert_eq!(
            gutter_cell.fg,
            cluster_color_for_token("2").to_ratatui_color(),
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
        app.detail_tab = DetailTab::Bigkeys;

        let backend = TestBackend::new(100, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_status_bar(frame, &app, frame.area()))
            .expect("status bar draw succeeds");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("[S]ummary")));
        assert!(!lines.iter().any(|line| line.contains("[L]atency")));
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
        app.detail_tab = DetailTab::Summary;

        let backend = TestBackend::new(100, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(detail_tabs_widget(&app), frame.area()))
            .expect("tab draw succeeds");

        let buffer = terminal.backend().buffer().clone();
        let lines = buffer_lines(&buffer);
        assert!(lines.iter().any(|line| line.contains("[S]ummary")));
        assert!(!lines.iter().any(|line| line.contains("[L]atency")));
        assert!(lines.iter().any(|line| line.contains("[I]nfo Raw")));
        assert!(lines.iter().any(|line| line.contains("[C]ommandstats")));
        assert!(lines.iter().any(|line| line.contains("[B]igkeys")));
        assert!(lines.iter().any(|line| line.contains("Hot[k]eys")));

        let line_index = lines
            .iter()
            .position(|line| line.contains("[S]ummary"))
            .expect("summary tab rendered");
        let tab_row = &lines[line_index];
        let width = usize::from(buffer.area.width);
        let summary_col = char_column(tab_row, "[S]ummary");
        let summary_idx = line_index * width + summary_col;

        assert_eq!(buffer.content()[summary_idx].symbol(), "[");
        assert_eq!(buffer.content()[summary_idx].fg, background_color(&app));
        assert_eq!(buffer.content()[summary_idx].bg, carat_color(&app));
        assert!(
            buffer.content()[summary_idx]
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn commandstats_compact_toggle_preserves_columns_and_respects_input_context() {
        let mut app = AppState::new(default_settings(), test_registry());
        let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.active_view = ActiveView::Detail;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.detail_tab = DetailTab::Commandstats;
        let columns = app.visible_commandstats_columns();
        app.active_pane_mut().scroll.viewport(20, 5);
        app.active_pane_mut().scroll.to_end();
        assert!(handle_commandstats_shortcut(&mut app, key));
        assert!(app.commandstats_compact);
        assert_eq!(app.active_pane().scroll.offset(), 0);
        app.active_pane_mut().is_filtering = true;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.active_pane_mut().is_filtering = false;
        app.show_help = true;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.show_help = false;
        app.open_column_picker();
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.close_overview_modal();
        assert!(!handle_commandstats_shortcut(
            &mut app,
            KeyEvent::new(key.code, KeyModifiers::CONTROL)
        ));
        assert!(!handle_commandstats_shortcut(
            &mut app,
            KeyEvent::new_with_kind(key.code, key.modifiers, KeyEventKind::Release)
        ));
        app.close_detail_view();
        assert!(app.commandstats_compact);
        app.active_view = ActiveView::Detail;
        assert!(handle_commandstats_shortcut(
            &mut app,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)
        ));
        assert!(!app.commandstats_compact);
        assert_eq!(app.visible_commandstats_columns(), columns);
    }

    #[test]
    fn commandstats_compact_packs_sorted_rows_and_scrolls_after_resize_and_filter() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.commandstats_compact = true;
        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.detail.commandstats = (0..31)
            .rev()
            .map(|idx| CommandStat {
                command: format!("cmd{idx:02}"),
                calls: 100 - idx,
                usec: 10,
                usec_per_call: 1.0,
                additional_metrics: std::collections::BTreeMap::default(),
            })
            .collect();
        app.apply_update(instance);
        // A 43-cell body fits three 13-cell pairs plus two 2-cell gaps exactly.
        let mut terminal = Terminal::new(TestBackend::new(45, 13)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let row = lines.iter().find(|line| line.contains("cmd00")).unwrap();
        assert!(row.find("cmd00").unwrap() < row.find("cmd01").unwrap());
        assert!(row.find("cmd01").unwrap() < row.find("cmd02").unwrap());
        assert!(row.contains("100") && row.contains("99") && row.contains("98"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Commandstats 1-15 / 31"))
        );
        assert!(!lines.iter().any(|line| line.contains("Usec")));
        app.active_pane_mut().scroll.page_by(1);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("cmd15")));
        assert!(!lines.iter().any(|line| line.contains("cmd14")));
        app.active_pane_mut().scroll.to_end();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .iter()
                .any(|line| line.contains("cmd30"))
        );

        // One cell less only fits two pairs; navigation still reaches the last item.
        terminal.backend_mut().resize(44, 13);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        app.active_pane_mut().scroll.to_start();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let row = lines.iter().find(|line| line.contains("cmd00")).unwrap();
        assert!(row.contains("cmd01"));
        assert!(!row.contains("cmd02"));
        app.active_pane_mut().scroll.to_end();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .iter()
                .any(|line| line.contains("cmd30"))
        );
        app.active_pane_mut().filter = "cmd0".into();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.active_pane().scroll.offset(), 0);
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("cmd09")));
        assert!(!lines.iter().any(|line| line.contains("cmd10")));
        app.active_pane_mut().filter = "absent".into();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .iter()
                .any(|line| line.contains("No commandstats match"))
        );
        terminal.backend_mut().resize(5, 8);
        terminal.autoresize().unwrap();
        app.active_pane_mut().filter.clear();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    #[test]
    fn commandstats_totals_follow_selection_including_hidden_nodes() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        for (key, port, calls, usec) in
            [("a", 6379, 2, 10), ("b", 6380, 8, 10), ("c", 6381, 50, 500)]
        {
            let mut instance = InstanceState::new(key.into(), format!("127.0.0.1:{port}"));
            instance.detail.commandstats = vec![CommandStat {
                command: "get".into(),
                calls,
                usec,
                usec_per_call: crate::column::u64_to_f64(usec) / crate::column::u64_to_f64(calls),
                additional_metrics: std::collections::BTreeMap::default(),
            }];
            app.apply_update(instance);
        }
        app.select_server_range(&["a".into(), "b".into()]);
        app.filter = "6381".into();
        app.clamp_selection();
        assert_eq!(app.selected_key().as_deref(), Some("c"));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        for compact in [false, true] {
            app.commandstats_compact = compact;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            assert!(lines.iter().any(|line| line.contains("2 selected nodes")));
            assert!(lines.iter().any(|line| line.contains("2 nodes")));
            let row = lines.iter().find(|line| line.contains("get")).unwrap();
            assert!(row.contains("10"));
            assert!(!row.contains("50"));
            if !compact {
                assert!(row.contains("20") && row.contains("2.00"));
            }
        }
        app.clear_server_selection();
        app.select_server_range(&["a".into()]);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("127.0.0.1:6379")));
        assert!(
            lines
                .iter()
                .find(|line| line.contains("get"))
                .unwrap()
                .contains('2')
        );
        app.clear_server_selection();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("127.0.0.1:6381")));
        assert!(
            lines
                .iter()
                .find(|line| line.contains("get"))
                .unwrap()
                .contains("50")
        );
        app.select_server_range(&["a".into(), "b".into()]);
        app.filter = "no visible nodes".into();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            buffer_lines(terminal.backend().buffer())
                .iter()
                .any(|line| line.contains("2 selected nodes"))
        );
    }

    #[test]
    fn commandstats_column_shortcuts_open_only_in_the_active_pane() {
        let mut app = AppState::new(default_settings(), test_registry());
        for code in [
            KeyCode::Char('c'),
            KeyCode::Char('C'),
            KeyCode::Char('v'),
            KeyCode::Char('V'),
            KeyCode::F(7),
        ] {
            app.active_view = ActiveView::Detail;
            for tab in DetailTab::ALL {
                app.detail_tab = tab;
                let opened =
                    handle_commandstats_shortcut(&mut app, KeyEvent::new(code, KeyModifiers::NONE));
                assert_eq!(opened, tab == DetailTab::Commandstats);
                if opened {
                    assert_eq!(app.overview_modal, OverviewModal::ColumnPicker);
                    assert_eq!(app.column_picker_entries().len(), 4);
                    assert_eq!(app.column_picker_entries()[0].label, "Command");
                    app.close_overview_modal();
                }
            }
            app.detail_tab = DetailTab::Commandstats;
            app.active_view = ActiveView::Overview;
            assert!(!handle_commandstats_shortcut(
                &mut app,
                KeyEvent::new(code, KeyModifiers::NONE)
            ));
        }
    }

    #[test]
    fn commandstats_column_shortcuts_respect_filters_overlays_and_modifiers() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        app.pane_mut(DetailTab::Commandstats).is_filtering = true;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.pane_mut(DetailTab::Commandstats).is_filtering = false;
        app.show_help = true;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.show_help = false;
        app.overview_modal = OverviewModal::ColumnPicker;
        assert!(!handle_commandstats_shortcut(&mut app, key));
        app.close_overview_modal();
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert!(!handle_commandstats_shortcut(
                &mut app,
                KeyEvent::new_with_kind(key.code, key.modifiers, kind)
            ));
        }
        for modifier in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert!(!handle_commandstats_shortcut(
                &mut app,
                KeyEvent::new(key.code, modifier)
            ));
        }
        assert_eq!(app.overview_modal, OverviewModal::None);
    }

    #[test]
    fn commandstats_picker_renders_checkboxes_and_selected_columns_in_order() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.detail.commandstats = vec![CommandStat {
            command: "get".into(),
            calls: 123_456,
            usec: 987_654,
            usec_per_call: 7.89,
            additional_metrics: std::collections::BTreeMap::new(),
        }];
        app.apply_update(instance);
        app.open_column_picker();
        app.move_column_picker_selection(2);
        handle_column_picker_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        app.move_column_picker_selection(1);
        app.move_selected_column(-3);

        let mut terminal = Terminal::new(TestBackend::new(140, 20)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("picker draw");
        let lines = buffer_lines(terminal.backend().buffer());
        assert!(lines.iter().any(|line| line.contains("[x] Command")));
        assert!(lines.iter().any(|line| line.contains("[ ] Usec")));
        assert!(lines.iter().any(|line| line.contains("[x] Usec/Call")));

        handle_column_picker_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.active_view, ActiveView::Detail);
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("table draw");
        let lines = buffer_lines(terminal.backend().buffer());
        let header = lines
            .iter()
            .find(|line| line.contains("Usec/Call"))
            .expect("header");
        assert!(char_column(header, "Usec/Call") < char_column(header, "Command"));
        assert!(char_column(header, "Command") < char_column(header, "Calls"));
        let row = lines
            .iter()
            .find(|line| line.contains("123,456"))
            .expect("row");
        assert!(char_column(row, "7.89") < char_column(row, "get"));
        assert!(char_column(row, "get") < char_column(row, "123,456"));
        assert!(!row.contains("987,654"));
        assert!(lines.iter().any(|line| line.contains("[F7]Columns")));
    }

    #[test]
    fn commandstats_renders_discovered_metrics_right_aligned_with_missing_values() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.detail.commandstats =
            crate::parse::parse_commandstats(&crate::parse::parse_info(concat!(
                "# Commandstats\n",
                "cmdstat_get:calls=3,usec=9,usec_per_call=3.00,",
                "failed_calls=12,future_metric=18446744073709551616\n",
                "cmdstat_set:calls=2,usec=8,usec_per_call=4.00,failed_calls=1\n",
            )));
        app.apply_update(instance);
        app.open_column_picker();
        for label in ["failed_calls", "future_metric"] {
            app.column_picker_index = app
                .column_picker_entries()
                .iter()
                .position(|entry| entry.label == label)
                .expect("discovered metric");
            app.toggle_selected_column_visibility();
        }
        app.close_overview_modal();
        let mut terminal = Terminal::new(TestBackend::new(150, 16)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw");
        let lines = buffer_lines(terminal.backend().buffer());
        let header = lines
            .iter()
            .find(|line| line.contains("future_metric"))
            .expect("header");
        let get = lines
            .iter()
            .find(|line| line.contains("get"))
            .expect("get row");
        let set = lines
            .iter()
            .find(|line| line.contains("set"))
            .expect("set row");
        let failed_end = char_column(header, "failed_calls") + "failed_calls".len();
        let future_end = char_column(header, "future_metric") + "future_metric".len();
        assert_eq!(get.chars().nth(failed_end - 2), Some('1'));
        assert_eq!(get.chars().nth(failed_end - 1), Some('2'));
        assert_eq!(set.chars().nth(failed_end - 1), Some('1'));
        assert_eq!(char_column(get, "18446744073709551616") + 20, future_end);
        assert_eq!(set.chars().nth(future_end - 1), Some('-'));
    }

    #[test]
    fn detail_commandstats_tab_renders_sorted_table() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = vec![
            CommandStat {
                command: "echo".into(),
                calls: 2_057,
                usec: 49_361_425,
                usec_per_call: 23_996.80,
                additional_metrics: std::collections::BTreeMap::new(),
            },
            CommandStat {
                command: "lrange".into(),
                calls: 400_000,
                usec: 6_420_146,
                usec_per_call: 16.05,
                additional_metrics: std::collections::BTreeMap::new(),
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
        app.detail_tab = DetailTab::InfoRaw;
        app.pane_mut(DetailTab::InfoRaw).filter = "run_id".into();

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
    fn detail_summary_includes_latency_and_filters_it() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Summary;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_latency_ms = Some(1.25);
        instance.max_latency_ms = 2.5;
        instance.avg_latency_ms = 1.875;
        instance.latency_window = [2.5, 1.25].into();
        app.apply_update(instance);

        let mut terminal = Terminal::new(TestBackend::new(100, 28)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("detail draw succeeds");
        let rendered = buffer_lines(terminal.backend().buffer())
            .join("\n")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for row in [
            "used_memory : -",
            "last_latency_ms : 1.25",
            "max_latency_ms : 2.50",
            "avg_latency_ms : 1.88",
            "window_samples : 2",
        ] {
            assert!(rendered.contains(row), "missing summary row: {row}");
        }

        app.pane_mut(DetailTab::Summary).filter = "latency".into();
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("filtered detail draw succeeds");
        let rendered = buffer_lines(terminal.backend().buffer())
            .join("\n")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("Summary 1-3 / 3 filter=/latency"));
        assert!(rendered.contains("last_latency_ms : 1.25"));
        assert!(rendered.contains("max_latency_ms : 2.50"));
        assert!(rendered.contains("avg_latency_ms : 1.88"));
        assert!(!rendered.contains("used_memory"));
        assert!(!rendered.contains("window_samples"));
    }

    #[test]
    fn detail_summary_without_latency_samples_shows_defaults() {
        let instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        let body = super::detail_text_body(&instance, DetailTab::Summary)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains("last_latency_ms : -"));
        assert!(body.contains("max_latency_ms : 0.00"));
        assert!(body.contains("avg_latency_ms : 0.00"));
        assert!(body.contains("window_samples : 0"));
    }

    #[test]
    fn detail_summary_tab_pages_lines_with_scroll_offset() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = DetailTab::Summary;
        let scroll = &mut app.pane_mut(DetailTab::Summary).scroll;
        scroll.viewport(100, 1);
        scroll.scroll_by(2);

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
        assert!(lines.iter().any(|line| line.contains("Summary 3-9 / 18")));
        assert!(!lines.iter().any(|line| line.contains("status ")));
        assert!(!lines.iter().any(|line| line.contains("used_memory ")));
        assert!(lines.iter().any(|line| line.contains("used_memory_rss")));
        assert!(lines.iter().any(|line| line.contains("commands")));
        assert!(!lines.iter().any(|line| line.contains("master ")));
    }

    #[test]
    fn detail_commandstats_tab_filters_rows_and_shows_filter_in_title() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.pane_mut(DetailTab::Commandstats).filter = "ran".into();

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = vec![
            CommandStat {
                command: "echo".into(),
                calls: 2_057,
                usec: 49_361_425,
                usec_per_call: 23_996.80,
                additional_metrics: std::collections::BTreeMap::new(),
            },
            CommandStat {
                command: "lrange".into(),
                calls: 400_000,
                usec: 6_420_146,
                usec_per_call: 16.05,
                additional_metrics: std::collections::BTreeMap::new(),
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
        app.detail_tab = DetailTab::Commandstats;

        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.last_updated = Some(std::time::Instant::now());
        instance.detail.commandstats = (0..20)
            .map(|idx| CommandStat {
                command: format!("cmd{idx:02}"),
                calls: u64::try_from(100 - idx).expect("non-negative"),
                usec: 10,
                usec_per_call: 1.0,
                additional_metrics: std::collections::BTreeMap::new(),
            })
            .collect();
        app.apply_update(instance);

        // 18 rows leave a 13-row pane: two borders and a header around 10 rows.
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).expect("test terminal");
        let mut render = |app: &mut AppState| {
            terminal
                .draw(|frame| draw(frame, app))
                .expect("detail draw succeeds");
            buffer_lines(terminal.backend().buffer())
        };

        let lines = render(&mut app);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Commandstats 1-10 / 20"))
        );
        app.active_pane_mut().scroll.scroll_by(2);
        let lines = render(&mut app);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Commandstats 3-12 / 20"))
        );
        assert!(!lines.iter().any(|line| line.contains("cmd01")));
        assert!(lines.iter().any(|line| line.contains("cmd02")));
        assert!(lines.iter().any(|line| line.contains("cmd11")));
        assert!(!lines.iter().any(|line| line.contains("cmd12")));

        // The final rows must be reachable by scrolling.
        app.active_pane_mut().scroll.scroll_by(isize::MAX);
        let lines = render(&mut app);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Commandstats 11-20 / 20"))
        );
        assert!(lines.iter().any(|line| line.contains("cmd19")));
    }

    #[test]
    fn detail_bigkeys_tab_renders_single_panel_and_rows() {
        let mut app = crate::app::AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, crate::model::SortMode::Address),
        );
        app.active_view = crate::app::ActiveView::Detail;
        app.detail_tab = DetailTab::Bigkeys;

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
        app.detail_tab = DetailTab::Bigkeys;
        app.pane_mut(DetailTab::Bigkeys).filter = "hash".into();

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
        app.detail_tab = DetailTab::Bigkeys;
        app.pane_mut(DetailTab::Bigkeys).filter = "nomatch".into();

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
        app.detail_tab = DetailTab::Hotkeys;

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
        app.detail_tab = DetailTab::Hotkeys;
        app.pane_mut(DetailTab::Hotkeys).filter = "alp".into();

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
        app.detail_tab = DetailTab::Hotkeys;

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
        app.detail_tab = DetailTab::Summary;

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

        let backend = TestBackend::new(100, 20);
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

        let backend = TestBackend::new(100, 20);
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
