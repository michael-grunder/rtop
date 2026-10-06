use std::time::{Duration, Instant};

use super::{ActiveView, AppState, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, OverviewModal};

const RANGE_TIMEOUT: Duration = Duration::from_millis(500);
const FAMILY_HOLD: Duration = Duration::from_millis(700);
const CLUSTER_HOLD: Duration = Duration::from_millis(1500);
const FIRST_REPEAT_TIMEOUT: Duration = Duration::from_millis(1000);
const REPEAT_TIMEOUT: Duration = Duration::from_millis(200);

struct SpaceHold {
    started: Instant,
    last_event: Instant,
    repeated: bool,
    // Counted ranges consume repeats but do not expand into a topology selection.
    target: Option<(String, bool)>,
    stage: u8,
}

#[derive(Default)]
pub(super) struct Navigation {
    count: Option<usize>,
    recent_range: Option<(Instant, Vec<String>)>,
    space_hold: Option<SpaceHold>,
    reports_release: bool,
}

/// Where a navigation key moves the focus or scroll position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    /// Relative rows (negative is up).
    Rows(isize),
    /// Relative pages of the last rendered size.
    Pages(isize),
    /// Absolute zero-based row.
    Row(usize),
    Last,
    /// Relative detail tabs.
    Tabs(isize),
}

impl Motion {
    fn from_key(code: KeyCode, count: Option<usize>) -> Option<Self> {
        let steps = count
            .unwrap_or(1)
            .min(isize::MAX.cast_unsigned())
            .cast_signed();
        Some(match code {
            KeyCode::Char('j') | KeyCode::Down => Self::Rows(steps),
            KeyCode::Char('k') | KeyCode::Up => Self::Rows(-steps),
            KeyCode::Char('l') | KeyCode::Right => Self::Tabs(steps),
            KeyCode::Char('h') | KeyCode::Left => Self::Tabs(-steps),
            KeyCode::PageDown => Self::Pages(steps),
            KeyCode::PageUp => Self::Pages(-steps),
            KeyCode::Char('g') | KeyCode::Home => Self::Row(0),
            // Like vim, a counted G jumps to that (one-based) row.
            KeyCode::Char('G') | KeyCode::End => {
                count.map_or(Self::Last, |row| Self::Row(row.saturating_sub(1)))
            }
            _ => return None,
        })
    }

    /// The equivalent relative row movement within a list of `len` items.
    fn row_delta(self, current: usize, len: usize, page_len: usize) -> isize {
        let page = page_len.max(1).cast_signed();
        match self {
            Self::Rows(delta) => delta,
            Self::Pages(pages) => pages.saturating_mul(page),
            Self::Row(row) => row.cast_signed().saturating_sub(current.cast_signed()),
            Self::Last => len.cast_signed().saturating_sub(current.cast_signed()),
            Self::Tabs(_) => 0,
        }
    }
}

impl Navigation {
    pub(super) fn new(reports_release: bool) -> Self {
        Self {
            reports_release,
            ..Self::default()
        }
    }

    pub(super) fn cancel_space_hold(&mut self) {
        self.space_hold = None;
    }

    pub(super) fn tick(&mut self, app: &mut AppState, now: Instant) {
        // Without release reporting, advance only when another repeat arrives.
        if self.reports_release {
            self.advance_space_hold(app, now);
        }
    }

    fn advance_space_hold(&mut self, app: &mut AppState, now: Instant) {
        if !accepts_navigation(app)
            || app.active_view != ActiveView::Overview
            || app.overview_modal != OverviewModal::None
        {
            self.cancel_space_hold();
            return;
        }
        let Some(hold) = &mut self.space_hold else {
            return;
        };
        let Some((key, selected)) = &hold.target else {
            return;
        };
        if !app.instances.contains_key(key) {
            self.cancel_space_hold();
            return;
        }
        let elapsed = now.saturating_duration_since(hold.started);
        if hold.stage == 0 && elapsed >= FAMILY_HOLD {
            app.expand_server_selection(key, false, *selected);
            hold.stage = 1;
        }
        if hold.stage == 1 && elapsed >= CLUSTER_HOLD {
            app.expand_server_selection(key, true, *selected);
            hold.stage = 2;
        }
    }

    fn handle_space(&mut self, app: &mut AppState, key: KeyEvent, now: Instant) -> bool {
        if let Some(hold) = &mut self.space_hold {
            let timeout = if hold.repeated {
                REPEAT_TIMEOUT
            } else {
                FIRST_REPEAT_TIMEOUT
            };
            // ponytail: legacy terminals cannot distinguish rapid taps from auto-repeat;
            // release-reporting terminals use exact events instead of this timing heuristic.
            let repeat = self.reports_release
                || key.kind == KeyEventKind::Repeat
                || now.saturating_duration_since(hold.last_event) <= timeout;
            if repeat {
                hold.last_event = now;
                hold.repeated = true;
                self.advance_space_hold(app, now);
                return true;
            }
            self.cancel_space_hold();
        }
        let count = self.count.take();
        let recent_range = self.recent_range.take();
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let mut hold = SpaceHold {
            started: now,
            last_event: now,
            repeated: false,
            target: None,
            stage: 0,
        };
        if let Some(count) = count {
            app.select_server_range(&range_keys(app, count, false));
        } else if let Some((at, keys)) = recent_range
            && now.saturating_duration_since(at) <= RANGE_TIMEOUT
        {
            app.select_server_range(&keys);
        } else {
            hold.target = app.selected_key().map(|key| {
                let selected = !app.is_server_selected(&key);
                (key, selected)
            });
            app.toggle_server_selection();
        }
        self.space_hold = Some(hold);
        true
    }

    /// Runs before command dispatch, but leaves text fields and non-navigation overlays alone.
    pub(super) fn handle_key(&mut self, app: &mut AppState, key: KeyEvent, now: Instant) -> bool {
        if !accepts_navigation(app) {
            *self = Self::new(self.reports_release);
            return false;
        }
        if key.code == KeyCode::Char(' ') && key.kind == KeyEventKind::Release {
            self.cancel_space_hold();
        }
        // Releases and standalone modifiers must not break a typed sequence.
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return false;
        }
        if super::has_command_modifier(key) {
            *self = Self::new(self.reports_release);
            return false;
        }
        let overview =
            app.active_view == ActiveView::Overview && app.overview_modal == OverviewModal::None;
        if key.code == KeyCode::Char(' ') && overview {
            return self.handle_space(app, key, now);
        }
        self.cancel_space_hold();
        if let KeyCode::Char(ch @ '0'..='9') = key.code {
            self.recent_range = None;
            let digit = usize::from(ch as u8 - b'0');
            // Counts start at 1; saturating arithmetic keeps arbitrarily long input safe.
            if digit != 0 || self.count.is_some() {
                self.count = Some(
                    self.count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit),
                );
            }
            return true;
        }
        let count = self.count.take();
        self.recent_range = None;
        if key.code == KeyCode::Esc && count.is_some() {
            return true;
        }
        let Some(motion) = Motion::from_key(key.code, count) else {
            return false;
        };
        if let Motion::Tabs(steps) = motion {
            if app.active_view == ActiveView::Detail && app.overview_modal == OverviewModal::None {
                app.set_detail_tab(app.detail_tab.rotate(steps));
            }
            return true;
        }
        match app.overview_modal {
            OverviewModal::SortPicker => {
                let len = app.sortable_columns().len();
                app.move_sort_picker_selection(motion.row_delta(app.sort_picker_index, len, len));
            }
            OverviewModal::ColumnPicker => {
                let reorder = key.modifiers.contains(KeyModifiers::SHIFT);
                app.set_column_picker_reorder_mode(reorder);
                let len = app.column_picker_entries().len();
                let delta = motion.row_delta(app.column_picker_index, len, len);
                if reorder {
                    app.move_selected_column(delta);
                } else {
                    app.move_column_picker_selection(delta);
                }
            }
            OverviewModal::KillPicker => {
                let len = crate::model::KillAction::ALL.len();
                app.move_kill_picker_selection(motion.row_delta(app.kill_picker_index, len, len));
            }
            OverviewModal::None if overview => {
                let len = app.visible_rows().len();
                let delta = motion.row_delta(app.selected_index, len, app.overview_page_len);
                if count.is_some()
                    && key.kind == KeyEventKind::Press
                    && let Motion::Rows(rows) = motion
                {
                    self.recent_range = Some((now, range_keys(app, count.unwrap_or(1), rows < 0)));
                }
                app.move_selection(delta);
            }
            OverviewModal::None => scroll_detail(app, motion),
            _ => {}
        }
        true
    }
}

fn accepts_navigation(app: &AppState) -> bool {
    !(app.is_auth_form_open()
        || app.is_filtering
        || app.editing_pane().is_some()
        || app.show_help
        || app.active_view == ActiveView::Help
        || app.overview_modal == OverviewModal::KillConfirmation)
}

fn range_keys(app: &AppState, count: usize, upward: bool) -> Vec<String> {
    let rows = app.visible_rows();
    let keys = |iter: &mut dyn Iterator<Item = &crate::app::DisplayRow>| {
        iter.take(count).map(|row| row.key.clone()).collect()
    };
    if upward {
        keys(&mut rows.iter().take(app.selected_index.saturating_add(1)).rev())
    } else {
        keys(&mut rows.iter().skip(app.selected_index))
    }
}

fn scroll_detail(app: &mut AppState, motion: Motion) {
    let scroll = &mut app.active_pane_mut().scroll;
    match motion {
        Motion::Rows(delta) => scroll.scroll_by(delta),
        Motion::Pages(pages) => scroll.page_by(pages),
        Motion::Row(row) => {
            scroll.to_start();
            scroll.scroll_by(row.min(isize::MAX.cast_unsigned()).cast_signed());
        }
        Motion::Last => scroll.to_end(),
        Motion::Tabs(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DetailTab;
    use crate::config::default_settings;
    use crate::model::{InstanceState, SortDirection, SortMode};
    use crate::registry::ColumnRegistry;

    fn app() -> AppState {
        let mut app = AppState::new(
            default_settings(),
            ColumnRegistry::load(None, true, SortMode::Address),
        );
        for port in 6379..6399 {
            let addr = format!("127.0.0.1:{port}");
            app.apply_update(InstanceState::new(addr.clone(), addr));
        }
        app
    }

    fn type_keys(nav: &mut Navigation, app: &mut AppState, keys: &str, at: Instant) {
        for ch in keys.chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            if !nav.handle_key(app, key, at) {
                super::super::handle_overview_shortcut(app, key);
            }
            nav.handle_key(
                app,
                KeyEvent::new_with_kind(key.code, key.modifiers, KeyEventKind::Release),
                at,
            );
        }
    }

    fn cluster_app(focus: &str) -> AppState {
        let mut app = app();
        app.instances.clear();
        for (key, parent, cluster) in [
            ("p1", None, "cluster-a"),
            ("r1", Some("p1"), "cluster-a"),
            ("r2", Some("p1:6379"), "cluster-a"),
            ("p2", None, "cluster-a"),
            ("r3", Some("p2"), "cluster-a"),
            ("other", None, "cluster-b"),
        ] {
            let mut node = InstanceState::new(key.into(), format!("{key}:6379"));
            node.kind = if parent.is_some() {
                crate::model::InstanceType::Replica
            } else {
                crate::model::InstanceType::Primary
            };
            node.parent_addr = parent.map(str::to_string);
            node.cluster_id = Some(cluster.into());
            app.apply_update(node);
        }
        app.selected_index = app
            .visible_rows()
            .iter()
            .position(|row| row.key == focus)
            .unwrap();
        app
    }

    fn space(nav: &mut Navigation, app: &mut AppState, kind: KeyEventKind, at: Instant) {
        nav.handle_key(
            app,
            KeyEvent::new_with_kind(KeyCode::Char(' '), KeyModifiers::NONE, kind),
            at,
        );
    }

    #[test]
    fn space_hold_steps_through_family_and_cluster_in_both_directions() {
        for focus in ["p1", "r1", "r2"] {
            for deselect in [false, true] {
                let mut app = cluster_app(focus);
                // Start with a mixed family to ensure expansion sets rather than toggles.
                app.expand_server_selection("other", true, true);
                if deselect {
                    app.expand_server_selection(focus, true, true);
                } else {
                    app.select_server_range(&["r2".into()]);
                    app.selected_index = app
                        .visible_rows()
                        .iter()
                        .position(|row| row.key == focus)
                        .unwrap();
                    if focus == "r2" {
                        app.toggle_server_selection();
                    }
                }
                let mut nav = Navigation::new(true);
                let now = Instant::now();
                space(&mut nav, &mut app, KeyEventKind::Press, now);
                assert_eq!(app.is_server_selected(focus), !deselect);
                let initial_count = app.selected_server_count();
                // Windows reports held key-down events as Press until a Release.
                space(
                    &mut nav,
                    &mut app,
                    KeyEventKind::Press,
                    now + Duration::from_millis(500),
                );
                assert_eq!(app.selected_server_count(), initial_count);
                nav.tick(&mut app, now + FAMILY_HOLD - Duration::from_millis(1));
                assert_eq!(app.selected_server_count(), initial_count);
                // A refresh can reorder the focus without changing the held target.
                app.sort_direction = SortDirection::Desc;
                nav.tick(&mut app, now + FAMILY_HOLD);
                for key in ["p1", "r1", "r2"] {
                    assert_eq!(app.is_server_selected(key), !deselect, "{focus}: {key}");
                }
                assert_eq!(app.is_server_selected("p2"), deselect);
                assert_eq!(app.is_server_selected("r3"), deselect);
                nav.tick(&mut app, now + CLUSTER_HOLD);
                assert_eq!(app.is_server_selected("p2"), !deselect);
                assert_eq!(app.is_server_selected("r3"), !deselect);
                assert!(app.is_server_selected("other"));
                let count = app.selected_server_count();
                space(
                    &mut nav,
                    &mut app,
                    KeyEventKind::Repeat,
                    now + CLUSTER_HOLD * 2,
                );
                assert_eq!(app.selected_server_count(), count);
            }
        }
    }

    #[test]
    fn short_taps_and_release_after_family_stop_expansion() {
        let mut app = cluster_app("r1");
        let mut nav = Navigation::new(true);
        let now = Instant::now();
        for expected in [true, false, true] {
            space(&mut nav, &mut app, KeyEventKind::Press, now);
            space(
                &mut nav,
                &mut app,
                KeyEventKind::Release,
                now + Duration::from_millis(50),
            );
            nav.tick(&mut app, now + CLUSTER_HOLD);
            assert_eq!(app.is_server_selected("r1"), expected);
            assert_eq!(app.selected_server_count(), usize::from(expected));
        }
        app.expand_server_selection("r1", true, true);
        space(&mut nav, &mut app, KeyEventKind::Press, now);
        nav.tick(&mut app, now + FAMILY_HOLD);
        space(&mut nav, &mut app, KeyEventKind::Release, now + FAMILY_HOLD);
        nav.tick(&mut app, now + CLUSTER_HOLD);
        assert_eq!(app.action_target_keys(), ["p2", "r3"]);
    }

    #[test]
    fn legacy_space_repeats_expand_without_idle_timer_or_repeated_toggles() {
        let mut app = cluster_app("r1");
        let mut nav = Navigation::new(false);
        let now = Instant::now();
        space(&mut nav, &mut app, KeyEventKind::Press, now);
        nav.tick(&mut app, now + FAMILY_HOLD);
        assert_eq!(app.selected_server_count(), 1);
        for ms in (500..=1500).step_by(100) {
            space(
                &mut nav,
                &mut app,
                KeyEventKind::Press,
                now + Duration::from_millis(ms),
            );
            assert_eq!(
                app.selected_server_count(),
                if ms < 700 {
                    1
                } else if ms < 1500 {
                    3
                } else {
                    5
                }
            );
        }
        space(
            &mut nav,
            &mut app,
            KeyEventKind::Press,
            now + Duration::from_secs(2),
        );
        assert!(!app.is_server_selected("r1"));
        assert_eq!(app.selected_server_count(), 4);
        nav.tick(&mut app, now + Duration::from_secs(4));
        assert_eq!(app.selected_server_count(), 4);
    }

    #[test]
    fn space_hold_cancels_on_commands_overlays_focus_loss_and_removed_target() {
        for scenario in 0..5 {
            let mut app = cluster_app("r1");
            let mut nav = Navigation::new(true);
            let now = Instant::now();
            space(&mut nav, &mut app, KeyEventKind::Press, now);
            match scenario {
                0 => {
                    type_keys(&mut nav, &mut app, "j", now);
                }
                1 => app.open_sort_picker(),
                2 => app.is_filtering = true,
                3 => nav.cancel_space_hold(),
                _ => app.remove_instance("r1"),
            }
            nav.tick(&mut app, now + CLUSTER_HOLD);
            assert!(app.selected_server_count() <= 1);
            assert!(!app.is_server_selected("p1"));
        }
    }

    #[test]
    fn held_counted_space_does_not_expand_or_toggle_its_range() {
        for reports_release in [false, true] {
            let mut app = cluster_app("p1");
            let mut nav = Navigation::new(reports_release);
            let now = Instant::now();
            type_keys(&mut nav, &mut app, "2", now);
            space(&mut nav, &mut app, KeyEventKind::Press, now);
            let selected = app.action_target_keys();
            for ms in (500..=2000).step_by(100) {
                space(
                    &mut nav,
                    &mut app,
                    if reports_release {
                        KeyEventKind::Repeat
                    } else {
                        KeyEventKind::Press
                    },
                    now + Duration::from_millis(ms),
                );
                nav.tick(&mut app, now + Duration::from_millis(ms));
                assert_eq!(app.action_target_keys(), selected);
            }
        }
    }

    #[test]
    fn topology_selection_includes_hidden_nodes_and_isolates_non_cluster_families() {
        for cluster_id in [None, Some(crate::model::STANDALONE_CLUSTER.into())] {
            let mut app = cluster_app("p1");
            for node in app.instances.values_mut() {
                node.cluster_id = cluster_id.clone();
            }
            app.view_mode = crate::model::ViewMode::Primary;
            app.filter = "p1".into();
            app.expand_server_selection("p1", false, true);
            assert_eq!(app.action_target_keys(), ["p1", "r1", "r2"]);
            app.expand_server_selection("p1", true, true);
            assert_eq!(app.action_target_keys(), ["p1", "r1", "r2"]);
            app.expand_server_selection("r2", false, false);
            assert_eq!(app.selected_server_count(), 0);
            app.expand_server_selection("missing", true, true);
            assert_eq!(app.selected_server_count(), 0);
            // Unmonitored primaries still have selectable sibling replicas.
            app.remove_instance("p1");
            app.instances.get_mut("r1").unwrap().parent_addr = Some("p1:6379".into());
            app.expand_server_selection("r1", false, true);
            assert_eq!(app.action_target_keys(), ["r1", "r2"]);
        }
    }

    #[test]
    fn counts_move_immediately_and_clamp_without_overflow() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "12j", now);
        assert_eq!(app.selected_index, 12);
        type_keys(&mut nav, &mut app, "3k", now);
        assert_eq!(app.selected_index, 9);
        assert_eq!(app.overview_modal, OverviewModal::None);
        type_keys(&mut nav, &mut app, "2", now);
        assert!(nav.handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            now
        ));
        assert_eq!(app.selected_index, 11);
        type_keys(&mut nav, &mut app, &format!("{}j", "9".repeat(100)), now);
        assert_eq!(app.selected_index, 19);
        type_keys(&mut nav, &mut app, &format!("{}k", "9".repeat(100)), now);
        assert_eq!(app.selected_index, 0);
        type_keys(&mut nav, &mut app, "0j", now);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn counted_space_adds_a_range_and_plain_space_still_toggles() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, " ", now);
        type_keys(&mut nav, &mut app, "4 ", now);
        assert_eq!(app.selected_server_count(), 4);
        assert_eq!(app.selected_index, 3);
        for port in 6379..6383 {
            assert!(app.is_server_selected(&format!("127.0.0.1:{port}")));
        }
        type_keys(&mut nav, &mut app, " ", now);
        assert_eq!(app.selected_server_count(), 3);
        app.selected_index = 18;
        type_keys(&mut nav, &mut app, "4 ", now);
        assert_eq!(app.selected_index, 19);
        assert_eq!(app.selected_server_count(), 5);
    }

    #[test]
    fn quick_space_selects_from_original_focus_in_both_directions() {
        for (motion, expected, end) in [('k', [6382, 6383, 6384], 3), ('j', [6384, 6385, 6386], 7)]
        {
            let mut app = app();
            app.selected_index = 5;
            let mut nav = Navigation::default();
            let now = Instant::now();
            type_keys(&mut nav, &mut app, &format!("3{motion}"), now);
            assert_eq!(app.selected_index, if motion == 'k' { 2 } else { 8 });
            type_keys(&mut nav, &mut app, " ", now + RANGE_TIMEOUT);
            assert_eq!(app.selected_index, end);
            assert_eq!(app.selected_server_count(), 3);
            for port in expected {
                assert!(app.is_server_selected(&format!("127.0.0.1:{port}")));
            }
        }
    }

    #[test]
    fn delayed_space_toggles_new_focus_and_unrelated_commands_cancel_range() {
        for intervening in ["", "o"] {
            let mut app = app();
            app.selected_index = 5;
            let mut nav = Navigation::default();
            let now = Instant::now();
            type_keys(&mut nav, &mut app, "3k", now);
            type_keys(&mut nav, &mut app, intervening, now);
            let delay = if intervening.is_empty() {
                RANGE_TIMEOUT + Duration::from_millis(1)
            } else {
                Duration::ZERO
            };
            type_keys(&mut nav, &mut app, " ", now + delay);
            assert_eq!(app.selected_index, 2);
            assert_eq!(app.selected_server_count(), 1);
            assert!(app.is_server_selected("127.0.0.1:6381"));
        }
    }

    #[test]
    fn range_uses_captured_visible_order_and_skips_removed_servers() {
        let mut app = app();
        app.filter = "638".into();
        app.sort_direction = SortDirection::Desc;
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "3j", now);
        app.sort_direction = SortDirection::Asc;
        app.remove_instance("127.0.0.1:6388");
        type_keys(&mut nav, &mut app, " ", now);
        assert_eq!(app.selected_server_count(), 2);
        assert!(app.is_server_selected("127.0.0.1:6389"));
        assert!(app.is_server_selected("127.0.0.1:6387"));
        assert_eq!(app.selected_key().as_deref(), Some("127.0.0.1:6387"));
    }

    #[test]
    fn empty_lists_and_upward_boundaries_are_safe() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        app.selected_index = 1;
        type_keys(&mut nav, &mut app, "4k ", now);
        assert_eq!(app.selected_server_count(), 2);
        assert_eq!(app.selected_index, 0);
        app.clear_server_selection();
        app.filter = "no matching servers".into();
        type_keys(&mut nav, &mut app, "4j 4 ", now);
        assert_eq!(app.selected_server_count(), 0);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn pending_counts_cancel_on_escape_and_commands() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "12", now);
        assert!(nav.handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            now
        ));
        assert!(!app.should_quit);
        type_keys(&mut nav, &mut app, "j", now);
        assert_eq!(app.selected_index, 1);
        type_keys(&mut nav, &mut app, "12oj", now);
        assert_eq!(app.selected_index, 2);
        assert!(app.force_show_host);
        type_keys(&mut nav, &mut app, "O", now);
        assert!(!app.force_show_host);
    }

    #[test]
    fn motion_keys_and_counts_leave_text_fields_and_help_alone() {
        for context in 0..4 + DetailTab::ALL.len() {
            let mut app = app();
            match context {
                0 => app.is_filtering = true,
                1 => app.open_auth_form(),
                2 => app.show_help = true,
                3 => app.active_view = ActiveView::Help,
                tab => {
                    app.active_view = ActiveView::Detail;
                    app.detail_tab = DetailTab::ALL[tab - 4];
                    app.start_active_detail_filter_input(false);
                }
            }
            let mut nav = Navigation::default();
            for ch in "123hjkl ".chars() {
                assert!(!nav.handle_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    Instant::now()
                ));
            }
            assert_eq!(app.selected_index, 0);
            assert_eq!(app.selected_server_count(), 0);
        }
    }

    #[test]
    fn release_events_preserve_counts_and_modified_keys_cancel_them() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "3", now);
        assert!(!nav.handle_key(
            &mut app,
            KeyEvent::new_with_kind(
                KeyCode::Char('3'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ),
            now
        ));
        type_keys(&mut nav, &mut app, "j", now);
        assert_eq!(app.selected_index, 3);
        assert!(!nav.handle_key(
            &mut app,
            KeyEvent::new_with_kind(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ),
            now
        ));
        type_keys(&mut nav, &mut app, " ", now);
        assert_eq!(app.selected_server_count(), 3);
        for modifier in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            type_keys(&mut nav, &mut app, "3", now);
            assert!(!nav.handle_key(&mut app, KeyEvent::new(KeyCode::Char('j'), modifier), now));
            let before = app.selected_index;
            type_keys(&mut nav, &mut app, "j", now);
            assert_eq!(app.selected_index, before + 1);
        }
    }

    #[test]
    fn detail_tabs_and_scrolling_accept_counts() {
        let mut app = app();
        app.active_view = ActiveView::Detail;
        // Scrolling clamps against the viewport the renderer last reported.
        app.pane_mut(DetailTab::Summary).scroll.viewport(10, 3);
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "2j", now);
        assert_eq!(app.pane(DetailTab::Summary).scroll.offset(), 2);
        type_keys(&mut nav, &mut app, "k", now);
        assert_eq!(app.pane(DetailTab::Summary).scroll.offset(), 1);
        type_keys(&mut nav, &mut app, "2l", now);
        assert_eq!(app.detail_tab, DetailTab::Commandstats);
        type_keys(&mut nav, &mut app, "3h", now);
        assert_eq!(app.detail_tab, DetailTab::Hotkeys);
        type_keys(&mut nav, &mut app, "l", now);
        assert_eq!(app.detail_tab, DetailTab::Summary);
    }

    #[test]
    fn paging_and_jumps_move_overview_focus_and_detail_scroll() {
        let mut app = app();
        app.overview_page_len = 5;
        let mut nav = Navigation::default();
        let now = Instant::now();
        let press = |nav: &mut Navigation, app: &mut AppState, code| {
            assert!(nav.handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), now));
        };
        press(&mut nav, &mut app, KeyCode::PageDown);
        assert_eq!(app.selected_index, 5);
        type_keys(&mut nav, &mut app, "G", now);
        assert_eq!(app.selected_index, 19);
        press(&mut nav, &mut app, KeyCode::PageUp);
        assert_eq!(app.selected_index, 14);
        type_keys(&mut nav, &mut app, "g", now);
        assert_eq!(app.selected_index, 0);
        type_keys(&mut nav, &mut app, "7G", now);
        assert_eq!(app.selected_index, 6);
        press(&mut nav, &mut app, KeyCode::End);
        assert_eq!(app.selected_index, 19);
        press(&mut nav, &mut app, KeyCode::Home);
        assert_eq!(app.selected_index, 0);

        app.active_view = ActiveView::Detail;
        app.pane_mut(DetailTab::Summary).scroll.viewport(30, 10);
        press(&mut nav, &mut app, KeyCode::PageDown);
        assert_eq!(app.pane(DetailTab::Summary).scroll.offset(), 10);
        press(&mut nav, &mut app, KeyCode::End);
        assert_eq!(app.pane(DetailTab::Summary).scroll.offset(), 20);
        type_keys(&mut nav, &mut app, "g", now);
        assert_eq!(app.pane(DetailTab::Summary).scroll.offset(), 0);
    }

    #[test]
    fn commandstats_picker_navigation_moves_columns_without_scrolling_or_switching_tabs() {
        use crate::commandstats::CommandstatsColumn::{Calls, Command, Usec, UsecPerCall};

        let mut app = app();
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.open_column_picker();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "2jkh", now);
        assert_eq!(app.column_picker_index, 1);
        assert_eq!(app.detail_tab, DetailTab::Commandstats);
        assert_eq!(app.pane(DetailTab::Commandstats).scroll.offset(), 0);
        assert!(nav.handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            now
        ));
        assert_eq!(app.column_picker_index, 2);
        assert_eq!(
            app.visible_commandstats_columns(),
            [Command, Usec, Calls, UsecPerCall]
        );
        assert_eq!(app.pane(DetailTab::Commandstats).scroll.offset(), 0);
        assert!(app.column_picker_reorder_mode);
    }

    #[test]
    fn pickers_accept_counts_without_selecting_servers_or_submitting_actions() {
        for modal in [
            OverviewModal::SortPicker,
            OverviewModal::ColumnPicker,
            OverviewModal::KillPicker,
        ] {
            let mut app = app();
            app.overview_modal = modal;
            let mut nav = Navigation::default();
            type_keys(&mut nav, &mut app, "3j2k", Instant::now());
            let index = match modal {
                OverviewModal::SortPicker => app.sort_picker_index,
                OverviewModal::ColumnPicker => app.column_picker_index,
                _ => app.kill_picker_index,
            };
            assert_eq!(index, 1);
            assert_eq!(app.overview_modal, modal);
            assert_eq!(app.selected_server_count(), 0);
        }
    }

    #[test]
    fn uppercase_k_opens_kill_while_lowercase_k_moves() {
        let mut app = app();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "jk", now);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.overview_modal, OverviewModal::None);
        type_keys(&mut nav, &mut app, "K", now);
        assert_eq!(app.overview_modal, OverviewModal::KillPicker);
    }

    #[test]
    fn counted_column_reordering_preserves_intervening_order() {
        let mut app = app();
        app.open_column_picker();
        let before = app.available_overview_columns();
        let mut nav = Navigation::default();
        let now = Instant::now();
        type_keys(&mut nav, &mut app, "3", now);
        assert!(nav.handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            now,
        ));
        let after = app.available_overview_columns();
        assert_eq!(app.column_picker_index, 3);
        assert_eq!(after[3], before[0]);
        assert_eq!(after[..3], before[1..4]);
        assert_eq!(after[4..], before[4..]);
    }

    #[test]
    fn held_motions_repeat_but_releases_do_not_move_picker_focus() {
        let mut app = app();
        app.open_sort_picker();
        let mut nav = Navigation::default();
        let now = Instant::now();
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            assert!(nav.handle_key(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, kind),
                now,
            ));
        }
        assert_eq!(app.sort_picker_index, 2);
        assert!(!nav.handle_key(
            &mut app,
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release),
            now,
        ));
        assert_eq!(app.sort_picker_index, 2);
    }
}
