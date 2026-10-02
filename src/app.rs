use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::column::{CellCtx, EmphasisLifetime, SortKey};
use crate::commandstats::CommandstatsColumn;
use crate::discovery::{DiscoveryEvent, DiscoveryStatus, VerifiedInstance};
use crate::hotkeys::{HotkeyEntry, HotkeysStatus};
use crate::model::{
    BigkeyEntry, CommandStat, InstanceState, InstanceType, KillAction, RuntimeSettings,
    SortDirection, ViewMode,
};
use crate::registry::ColumnRegistry;
use crate::target_addr::canonical_host;
use crate::topology::{TreeGroup, build_tree_groups};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Overview,
    Detail,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPromptMode {
    Search,
    Filter,
}

impl FilterPromptMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Filter => "Filter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewModal {
    None,
    SortPicker,
    ColumnPicker,
    KillPicker,
    KillConfirmation,
    AuthForm,
}

/// The panels of the detail view, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Summary,
    InfoRaw,
    Commandstats,
    Bigkeys,
    Hotkeys,
}

impl DetailTab {
    pub const ALL: [Self; 5] = [
        Self::Summary,
        Self::InfoRaw,
        Self::Commandstats,
        Self::Bigkeys,
        Self::Hotkeys,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::InfoRaw => "Info Raw",
            Self::Commandstats => "Commandstats",
            Self::Bigkeys => "Bigkeys",
            Self::Hotkeys => "Hotkeys",
        }
    }

    /// Lowercase mnemonic; it is also the highlighted letter in the title.
    pub const fn shortcut(self) -> char {
        match self {
            Self::Summary => 's',
            Self::InfoRaw => 'i',
            Self::Commandstats => 'c',
            Self::Bigkeys => 'b',
            Self::Hotkeys => 'k',
        }
    }

    /// Lowercase h/j/k/l are motions, so their tabs need the uppercase key.
    pub fn from_shortcut(ch: char) -> Option<Self> {
        if matches!(ch, 'h' | 'j' | 'k' | 'l') {
            return None;
        }
        let ch = ch.to_ascii_lowercase();
        Self::ALL.into_iter().find(|tab| tab.shortcut() == ch)
    }

    pub const fn index(self) -> usize {
        self as usize
    }

    /// Moves `steps` tabs forward (or backward when negative), wrapping around.
    pub const fn rotate(self, steps: isize) -> Self {
        let len = Self::ALL.len().cast_signed();
        let next = (self.index().cast_signed() + steps % len).rem_euclid(len);
        Self::ALL[next.cast_unsigned()]
    }

    /// Text tabs render a body of lines rather than a table.
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Summary | Self::InfoRaw)
    }
}

pub struct ColumnPickerEntry {
    pub label: String,
    pub visible: bool,
    pub suffix: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthField {
    Username,
    Password,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthFormState {
    pub target_keys: Vec<String>,
    pub username: String,
    pub password: String,
    pub active_field: AuthField,
}

#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub key: String,
    pub tree_prefix: String,
    pub stale: bool,
}

/// Returns `current` moved by `delta`, clamped to `0..len` (0 when empty).
pub const fn step_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = current.saturating_add_signed(delta);
    if next >= len { len - 1 } else { next }
}

/// Scroll position of a paged list. The renderer reports the content and
/// page sizes it actually used, so input handling can clamp against them
/// without re-deriving the layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scroll {
    offset: usize,
    page_len: usize,
    content_len: usize,
}

impl Scroll {
    pub const fn offset(&self) -> usize {
        self.offset
    }

    const fn max_offset(&self) -> usize {
        let page_len = if self.page_len == 0 { 1 } else { self.page_len };
        self.content_len.saturating_sub(page_len)
    }

    pub const fn scroll_by(&mut self, delta: isize) {
        self.offset = step_index(self.offset, delta, self.max_offset() + 1);
    }

    /// Scrolls by whole pages of the last rendered size.
    pub const fn page_by(&mut self, pages: isize) {
        let page = if self.page_len == 0 { 1 } else { self.page_len };
        self.scroll_by(pages.saturating_mul(page.cast_signed()));
    }

    pub const fn to_start(&mut self) {
        self.offset = 0;
    }

    pub const fn to_end(&mut self) {
        self.offset = self.max_offset();
    }

    /// Records the rendered dimensions and returns the visible item range.
    pub fn viewport(&mut self, content_len: usize, page_len: usize) -> Range<usize> {
        self.content_len = content_len;
        self.page_len = page_len.max(1);
        self.offset = self.offset.min(self.max_offset());
        self.offset..(self.offset + self.page_len).min(content_len)
    }
}

/// Case-insensitive substring matcher shared by every filter prompt.
pub struct Matcher {
    needle: String,
}

impl Matcher {
    pub fn new(filter: &str) -> Self {
        Self {
            needle: filter.trim().to_ascii_lowercase(),
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        self.needle.is_empty() || text.to_ascii_lowercase().contains(&self.needle)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailPaneState {
    pub filter: String,
    pub is_filtering: bool,
    pub scroll: Scroll,
}

impl DetailPaneState {
    pub fn matcher(&self) -> Matcher {
        Matcher::new(&self.filter)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Ordered columns with a visible subset, as edited by the column picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSet<T> {
    pub order: Vec<T>,
    pub visible: Vec<T>,
}

impl<T: PartialEq + Clone> ColumnSet<T> {
    pub fn new(order: Vec<T>, visible: impl IntoIterator<Item = T>) -> Self {
        let mut deduped: Vec<T> = Vec::new();
        for column in visible {
            if order.contains(&column) && !deduped.contains(&column) {
                deduped.push(column);
            }
        }
        Self {
            order,
            visible: deduped,
        }
    }

    pub fn is_visible(&self, column: &T) -> bool {
        self.visible.contains(column)
    }

    pub fn visible_in_order(&self) -> impl Iterator<Item = &T> {
        self.order.iter().filter(|column| self.is_visible(column))
    }

    /// Appends a newly discovered column without changing focus or visibility.
    pub fn discover(&mut self, column: T) {
        if !self.order.contains(&column) {
            self.order.push(column);
        }
    }

    /// Moves the column at `index` by `delta` and returns its new index.
    pub fn move_entry(&mut self, index: usize, delta: isize) -> usize {
        if index >= self.order.len() {
            return index;
        }
        let next = step_index(index, delta, self.order.len());
        let column = self.order.remove(index);
        self.order.insert(next, column);
        next
    }

    /// Shows or hides `column`; `can_hide` vetoes hiding (e.g. the last one).
    pub fn toggle(&mut self, column: &T, can_hide: impl FnOnce(&Self) -> bool) -> bool {
        if !self.is_visible(column) {
            self.visible.push(column.clone());
            return true;
        }
        if !can_hide(self) {
            return false;
        }
        self.visible.retain(|visible| visible != column);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnPickerTarget {
    Overview,
    Commandstats,
}

/// Values shared by every cell of one overview frame, computed once.
pub(crate) struct RowCtx {
    pub(crate) cluster_labels: HashMap<String, String>,
    omit_host: bool,
}

impl RowCtx {
    pub(crate) fn cell<'a>(&'a self, node: &'a InstanceState, tree_prefix: &'a str) -> CellCtx<'a> {
        CellCtx {
            snap: node,
            omit_host: self.omit_host,
            tree_prefix,
            cluster_label: self
                .cluster_labels
                .get(node.cluster_key())
                .map(String::as_str),
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AppState {
    pub settings: RuntimeSettings,
    pub view_mode: ViewMode,
    pub sort_by: String,
    pub sort_direction: SortDirection,
    pub overview_modal: OverviewModal,
    pub sort_picker_index: usize,
    pub column_picker_index: usize,
    pub kill_picker_index: usize,
    pub kill_target_keys: Vec<String>,
    pub auth_form: Option<AuthFormState>,
    pub column_picker_reorder_mode: bool,
    column_picker_target: ColumnPickerTarget,
    pub commandstats_columns: ColumnSet<CommandstatsColumn>,
    pub overview_columns: ColumnSet<String>,
    pub filter: String,
    pub is_filtering: bool,
    pub filter_prompt_mode: FilterPromptMode,
    pub show_help: bool,
    pub active_view: ActiveView,
    pub previous_view: ActiveView,
    pub selected_index: usize,
    /// Rows the overview table showed last frame; drives page-wise movement.
    pub overview_page_len: usize,
    marked_keys: BTreeSet<String>,
    pub detail_tab: DetailTab,
    detail_panes: [DetailPaneState; DetailTab::ALL.len()],
    pub force_show_host: bool,
    pub instances: HashMap<String, InstanceState>,
    pub discovery_status: DiscoveryStatus,
    pub should_quit: bool,
    pub column_registry: ColumnRegistry,
    hotkeys_locally_reset: HashSet<String>,
    pending_transient_emphasis: HashMap<String, String>,
    transient_emphasis_records: HashMap<String, SortKey>,
}

struct TreeRenderCtx<'a> {
    filtered_map: &'a HashMap<&'a str, &'a InstanceState>,
    group: &'a TreeGroup,
    rows: &'a RowCtx,
}

impl AppState {
    pub fn new(settings: RuntimeSettings, column_registry: ColumnRegistry) -> Self {
        Self {
            view_mode: settings.default_view,
            sort_by: column_registry.default_sort_by.clone(),
            sort_direction: column_registry.default_sort_direction,
            overview_modal: OverviewModal::None,
            sort_picker_index: 0,
            column_picker_index: 0,
            kill_picker_index: 0,
            kill_target_keys: Vec::new(),
            auth_form: None,
            column_picker_reorder_mode: false,
            column_picker_target: ColumnPickerTarget::Overview,
            commandstats_columns: ColumnSet::new(
                CommandstatsColumn::DEFAULT.to_vec(),
                CommandstatsColumn::DEFAULT,
            ),
            overview_columns: ColumnSet::new(
                column_registry.available_overview_columns(),
                column_registry.visible_overview.clone(),
            ),
            settings,
            filter: String::new(),
            is_filtering: false,
            filter_prompt_mode: FilterPromptMode::Filter,
            show_help: false,
            active_view: ActiveView::Overview,
            previous_view: ActiveView::Overview,
            selected_index: 0,
            overview_page_len: 0,
            marked_keys: BTreeSet::new(),
            detail_tab: DetailTab::Summary,
            detail_panes: Default::default(),
            force_show_host: false,
            instances: HashMap::new(),
            discovery_status: DiscoveryStatus::default(),
            should_quit: false,
            hotkeys_locally_reset: HashSet::new(),
            column_registry,
            pending_transient_emphasis: HashMap::new(),
            transient_emphasis_records: HashMap::new(),
        }
    }

    pub fn apply_update(&mut self, mut update: InstanceState) {
        // Append discoveries so polling never moves the picker focus or changes user ordering.
        let metrics: BTreeSet<_> = update
            .detail
            .commandstats
            .iter()
            .flat_map(|stat| stat.additional_metrics.keys())
            .collect();
        for metric in metrics {
            self.commandstats_columns
                .discover(CommandstatsColumn::Metric(metric.clone()));
        }
        let key = update.key.clone();
        if self.hotkeys_locally_reset.contains(&key)
            && update.detail.hotkeys.status != HotkeysStatus::Running
        {
            update.detail.hotkeys.reset();
        }
        self.instances.insert(key.clone(), update);
        self.track_transient_emphasis(&key);
        self.clamp_selection();
    }

    pub fn remove_instance(&mut self, key: &str) {
        self.instances.remove(key);
        self.marked_keys.remove(key);
        self.hotkeys_locally_reset.remove(key);
        self.pending_transient_emphasis
            .retain(|_, winner| winner != key);
        self.transient_emphasis_records.remove(key);
        self.clamp_selection();
    }

    pub fn reset_hotkeys_locally(&mut self, key: &str) {
        self.hotkeys_locally_reset.insert(key.to_string());
        if let Some(instance) = self.instances.get_mut(key) {
            instance.detail.hotkeys.reset();
        }
    }

    pub fn clear_hotkeys_local_reset(&mut self, key: &str) {
        self.hotkeys_locally_reset.remove(key);
    }

    pub fn apply_discovery_event(&mut self, event: &DiscoveryEvent) {
        self.discovery_status.apply_event(event);
    }

    pub fn apply_verified_instance(&mut self, verified: VerifiedInstance) {
        self.apply_update(verified.state);
    }

    pub fn selected_key(&self) -> Option<String> {
        self.visible_rows()
            .get(self.selected_index)
            .map(|row| row.key.clone())
    }

    pub fn selected_instance(&self) -> Option<&InstanceState> {
        self.instances.get(&self.selected_key()?)
    }

    pub fn toggle_server_selection(&mut self) {
        if let Some(key) = self.selected_key()
            && !self.marked_keys.remove(&key)
        {
            self.marked_keys.insert(key);
        }
    }

    pub fn is_server_selected(&self, key: &str) -> bool {
        self.marked_keys.contains(key)
    }

    /// Add a captured visible range, preserving existing selections and skipping removed servers.
    pub fn select_server_range(&mut self, keys: &[String]) {
        self.marked_keys.extend(
            keys.iter()
                .filter(|key| self.instances.contains_key(*key))
                .cloned(),
        );
        let rows = self.visible_rows();
        if let Some(index) = keys
            .iter()
            .rev()
            .find_map(|key| rows.iter().position(|row| &row.key == key))
        {
            self.selected_index = index;
        }
    }

    pub fn selected_server_count(&self) -> usize {
        self.marked_keys.len()
    }

    pub fn clear_server_selection(&mut self) {
        self.marked_keys.clear();
    }

    /// Explicit selections take precedence, including servers hidden by the current view.
    pub fn action_target_keys(&self) -> Vec<String> {
        if self.marked_keys.is_empty() {
            self.selected_key().into_iter().collect()
        } else {
            self.marked_keys.iter().cloned().collect()
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selected_index = step_index(self.selected_index, delta, self.visible_rows().len());
    }

    /// Moves the overview focus by whole pages of the last rendered table.
    pub fn page_selection(&mut self, pages: isize) {
        let page = self.overview_page_len.max(1).cast_signed();
        self.move_selection(pages.saturating_mul(page));
    }

    pub fn clamp_selection(&mut self) {
        self.move_selection(0);
    }

    pub fn open_help_view(&mut self) {
        if self.active_view != ActiveView::Help {
            self.previous_view = self.active_view;
        }
        self.active_view = ActiveView::Help;
    }

    pub const fn close_help_view(&mut self) {
        self.active_view = self.previous_view;
    }

    pub fn start_filter_input(&mut self, mode: FilterPromptMode, clear_existing: bool) {
        self.filter_prompt_mode = mode;
        if clear_existing {
            self.filter.clear();
        }
        self.is_filtering = true;
        self.clamp_selection();
    }

    pub const fn pane(&self, tab: DetailTab) -> &DetailPaneState {
        &self.detail_panes[tab.index()]
    }

    pub const fn pane_mut(&mut self, tab: DetailTab) -> &mut DetailPaneState {
        &mut self.detail_panes[tab.index()]
    }

    pub const fn active_pane(&self) -> &DetailPaneState {
        self.pane(self.detail_tab)
    }

    pub const fn active_pane_mut(&mut self) -> &mut DetailPaneState {
        self.pane_mut(self.detail_tab)
    }

    pub fn is_detail_tab(&self, tab: DetailTab) -> bool {
        self.active_view == ActiveView::Detail && self.detail_tab == tab
    }

    /// The detail pane whose filter prompt currently owns keyboard input.
    pub fn editing_pane(&self) -> Option<DetailTab> {
        (self.active_view == ActiveView::Detail && self.active_pane().is_filtering)
            .then_some(self.detail_tab)
    }

    /// Switches detail tabs; an unfinished filter prompt does not follow.
    pub const fn set_detail_tab(&mut self, tab: DetailTab) {
        self.active_pane_mut().is_filtering = false;
        self.detail_tab = tab;
    }

    pub fn start_active_detail_filter_input(&mut self, clear_existing: bool) {
        let pane = self.active_pane_mut();
        if clear_existing {
            pane.filter.clear();
        }
        pane.is_filtering = true;
        pane.scroll.to_start();
    }

    pub fn clear_detail_filters(&mut self) {
        self.detail_panes
            .iter_mut()
            .for_each(DetailPaneState::reset);
    }

    pub fn close_detail_view(&mut self) {
        self.clear_detail_filters();
        self.active_view = ActiveView::Overview;
    }

    pub fn visible_commandstats<'a>(&self, stats: &'a [CommandStat]) -> Vec<&'a CommandStat> {
        let matcher = self.pane(DetailTab::Commandstats).matcher();
        let mut filtered: Vec<_> = stats
            .iter()
            .filter(|stat| matcher.matches(&stat.command))
            .collect();
        filtered.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.command.cmp(&right.command))
        });
        filtered
    }

    pub fn visible_bigkeys<'a>(&self, entries: &'a [BigkeyEntry]) -> Vec<&'a BigkeyEntry> {
        let matcher = self.pane(DetailTab::Bigkeys).matcher();
        entries
            .iter()
            .filter(|entry| matcher.matches(&entry.key) || matcher.matches(&entry.key_type))
            .collect()
    }

    pub fn visible_hotkeys<'a>(&self, entries: &'a [HotkeyEntry]) -> Vec<&'a HotkeyEntry> {
        let matcher = self.pane(DetailTab::Hotkeys).matcher();
        entries
            .iter()
            .filter(|entry| matcher.matches(&entry.key))
            .collect()
    }

    pub fn visible_detail_text_lines<'a>(&self, tab: DetailTab, body: &'a str) -> Vec<&'a str> {
        let matcher = self.pane(tab).matcher();
        body.lines().filter(|line| matcher.matches(line)).collect()
    }

    pub fn visible_rows(&self) -> Vec<DisplayRow> {
        let filter = Matcher::new(&self.filter);
        let mut nodes: Vec<&InstanceState> = self
            .instances
            .values()
            .filter(|node| matches_filter(&filter, node))
            .collect();
        let rows = self.row_ctx();

        if self.view_mode == ViewMode::Tree {
            return self.build_tree_rows(&nodes, &rows);
        }
        if self.view_mode == ViewMode::Primary {
            nodes.retain(|node| node.kind != InstanceType::Replica);
        }
        self.sort_nodes(&mut nodes, &rows, |_| 0);
        nodes
            .into_iter()
            .map(|node| self.to_display_row(node, ""))
            .collect()
    }

    pub fn visible_column_keys(&self) -> Vec<String> {
        self.overview_columns
            .visible_in_order()
            .filter(|key| self.column_registry.column(key).is_some())
            .filter(|key| !self.is_column_auto_hidden(key))
            .cloned()
            .collect()
    }

    pub fn show_address_column(&self) -> bool {
        !self.should_omit_host_in_rendering()
    }

    pub const fn toggle_host_rendering(&mut self) {
        self.force_show_host = !self.force_show_host;
    }

    pub fn sortable_columns(&self) -> Vec<String> {
        self.visible_column_keys()
    }

    pub fn available_overview_columns(&self) -> Vec<String> {
        self.overview_columns
            .order
            .iter()
            .filter(|key| self.column_registry.column(key).is_some())
            .cloned()
            .collect()
    }

    pub fn column_auto_hidden_suffix(&self, column_key: &str) -> Option<&'static str> {
        match column_key {
            "addr" if self.is_column_auto_hidden(column_key) => Some(" (auto hidden)"),
            "role" if self.is_column_auto_hidden(column_key) => Some(" (auto hidden in Tree)"),
            _ => None,
        }
    }

    pub fn column_label(&self, key: &str) -> String {
        self.column_registry
            .column(key)
            .map_or_else(|| key.to_string(), |column| column.header().to_string())
    }

    pub fn sort_label(&self) -> String {
        self.column_label(&self.sort_by)
    }

    pub fn open_sort_picker(&mut self) {
        self.sort_picker_index = self
            .sortable_columns()
            .iter()
            .position(|key| *key == self.sort_by)
            .unwrap_or(0);
        self.overview_modal = OverviewModal::SortPicker;
    }

    pub fn open_column_picker(&mut self) {
        self.column_picker_target = if self.is_detail_tab(DetailTab::Commandstats) {
            ColumnPickerTarget::Commandstats
        } else {
            ColumnPickerTarget::Overview
        };
        self.column_picker_index = self
            .column_picker_entries()
            .iter()
            .position(|entry| entry.visible)
            .unwrap_or(0);
        self.column_picker_reorder_mode = false;
        self.overview_modal = OverviewModal::ColumnPicker;
    }

    pub fn visible_commandstats_columns(&self) -> Vec<CommandstatsColumn> {
        self.commandstats_columns
            .visible_in_order()
            .cloned()
            .collect()
    }

    pub fn column_picker_entries(&self) -> Vec<ColumnPickerEntry> {
        match self.column_picker_target {
            ColumnPickerTarget::Commandstats => self
                .commandstats_columns
                .order
                .iter()
                .map(|column| ColumnPickerEntry {
                    label: column.header().to_string(),
                    visible: self.commandstats_columns.is_visible(column),
                    suffix: "",
                })
                .collect(),
            ColumnPickerTarget::Overview => self
                .available_overview_columns()
                .into_iter()
                .map(|key| ColumnPickerEntry {
                    label: self.column_label(&key),
                    visible: self.is_column_visible(&key),
                    suffix: self
                        .column_auto_hidden_suffix(&key)
                        .unwrap_or(if key == self.sort_by { " (sort)" } else { "" }),
                })
                .collect(),
        }
    }

    pub fn open_kill_picker(&mut self) {
        self.kill_target_keys = self.action_target_keys();
        if self.kill_target_keys.is_empty() {
            return;
        }
        self.kill_picker_index = 0;
        self.overview_modal = OverviewModal::KillPicker;
    }

    /// Freeze targets when the picker opens so polling and sorting cannot retarget a stop.
    pub fn submit_kill(&mut self) -> Option<(Vec<String>, KillAction)> {
        if !matches!(
            self.overview_modal,
            OverviewModal::KillPicker | OverviewModal::KillConfirmation
        ) || self.kill_target_keys.is_empty()
        {
            return None;
        }
        let action = self.selected_kill_action()?;
        if self.overview_modal == OverviewModal::KillPicker && self.kill_target_keys.len() > 1 {
            self.overview_modal = OverviewModal::KillConfirmation;
            return None;
        }
        let keys = std::mem::take(&mut self.kill_target_keys);
        self.close_overview_modal();
        Some((keys, action))
    }

    pub fn open_auth_form(&mut self) {
        let target_keys = self.action_target_keys();
        if target_keys.is_empty() {
            return;
        }
        self.auth_form = Some(AuthFormState {
            target_keys,
            username: "default".to_string(),
            password: String::new(),
            active_field: AuthField::Username,
        });
        self.overview_modal = OverviewModal::AuthForm;
    }

    pub fn close_auth_form(&mut self) {
        self.discard_auth_form();
        self.overview_modal = OverviewModal::None;
    }

    fn discard_auth_form(&mut self) {
        if let Some(mut form) = self.auth_form.take() {
            form.password.clear();
        }
    }

    pub const fn toggle_auth_field(&mut self) {
        if let Some(form) = &mut self.auth_form {
            form.active_field = match form.active_field {
                AuthField::Username => AuthField::Password,
                AuthField::Password => AuthField::Username,
            };
        }
    }

    pub fn auth_active_value_mut(&mut self) -> Option<&mut String> {
        let form = self.auth_form.as_mut()?;
        match form.active_field {
            AuthField::Username => Some(&mut form.username),
            AuthField::Password => Some(&mut form.password),
        }
    }

    pub fn take_auth_credentials(&mut self) -> Option<(Vec<String>, Option<String>, String)> {
        let form = self.auth_form.as_ref()?;
        if form.password.is_empty() {
            return None;
        }
        let username = (!form.username.is_empty()).then(|| form.username.clone());
        let submission = (form.target_keys.clone(), username, form.password.clone());
        self.close_auth_form();
        Some(submission)
    }

    pub fn close_overview_modal(&mut self) {
        self.kill_target_keys.clear();
        self.column_picker_reorder_mode = false;
        self.discard_auth_form();
        self.overview_modal = OverviewModal::None;
    }

    pub fn move_sort_picker_selection(&mut self, delta: isize) {
        self.sort_picker_index =
            step_index(self.sort_picker_index, delta, self.sortable_columns().len());
    }

    pub fn apply_sort_picker_selection(&mut self) {
        let columns = self.sortable_columns();
        if let Some(chosen_key) = columns.get(self.sort_picker_index) {
            if self.sort_by == *chosen_key {
                self.sort_direction = self.sort_direction.toggle();
            } else {
                self.set_sort_column(chosen_key.clone());
            }
            self.clamp_selection();
        }
        self.overview_modal = OverviewModal::None;
    }

    pub fn move_column_picker_selection(&mut self, delta: isize) {
        self.column_picker_index = step_index(
            self.column_picker_index,
            delta,
            self.column_picker_entries().len(),
        );
    }

    pub const fn move_kill_picker_selection(&mut self, delta: isize) {
        self.kill_picker_index = step_index(self.kill_picker_index, delta, KillAction::ALL.len());
    }

    pub fn set_column_picker_reorder_mode(&mut self, enabled: bool) {
        self.column_picker_reorder_mode = enabled && self.is_column_picker_open();
    }

    pub fn move_selected_column(&mut self, delta: isize) {
        let index = self.column_picker_index;
        self.column_picker_index = match self.column_picker_target {
            ColumnPickerTarget::Commandstats => self.commandstats_columns.move_entry(index, delta),
            ColumnPickerTarget::Overview => {
                let Some(order_index) = self.picked_overview_column().and_then(|key| {
                    self.overview_columns
                        .order
                        .iter()
                        .position(|candidate| *candidate == key)
                }) else {
                    return;
                };
                self.overview_columns.move_entry(order_index, delta)
            }
        };
    }

    pub fn toggle_selected_column_visibility(&mut self) {
        match self.column_picker_target {
            ColumnPickerTarget::Commandstats => {
                let Some(column) = self
                    .commandstats_columns
                    .order
                    .get(self.column_picker_index)
                    .cloned()
                else {
                    return;
                };
                self.commandstats_columns
                    .toggle(&column, |set| set.visible.len() > 1);
            }
            ColumnPickerTarget::Overview => {
                let Some(key) = self.picked_overview_column() else {
                    return;
                };
                // Auto-hidden columns do not count towards the last visible one.
                let shown_after_hiding = self
                    .visible_column_keys()
                    .iter()
                    .filter(|visible| **visible != key)
                    .count();
                if self
                    .overview_columns
                    .toggle(&key, |_| shown_after_hiding > 0)
                {
                    self.ensure_sort_column_visible();
                    self.clamp_selection();
                }
            }
        }
    }

    fn picked_overview_column(&self) -> Option<String> {
        self.available_overview_columns()
            .into_iter()
            .nth(self.column_picker_index)
    }

    pub fn cycle_sort_mode(&mut self) {
        let columns = self.sortable_columns();
        if columns.is_empty() {
            return;
        }
        let current_idx = columns
            .iter()
            .position(|key| *key == self.sort_by)
            .unwrap_or(0);
        self.set_sort_column(columns[(current_idx + 1) % columns.len()].clone());
        self.clamp_selection();
    }

    pub fn cycle_view_mode(&mut self) {
        self.view_mode = self.view_mode.cycle();
        self.ensure_sort_column_visible();
        self.clamp_selection();
    }

    pub fn is_sort_picker_open(&self) -> bool {
        self.overview_modal == OverviewModal::SortPicker
    }

    pub fn is_column_picker_open(&self) -> bool {
        self.overview_modal == OverviewModal::ColumnPicker
    }

    pub fn is_kill_picker_open(&self) -> bool {
        self.overview_modal == OverviewModal::KillPicker
    }

    pub fn is_auth_form_open(&self) -> bool {
        self.overview_modal == OverviewModal::AuthForm
    }

    pub fn selected_kill_action(&self) -> Option<KillAction> {
        KillAction::ALL.get(self.kill_picker_index).copied()
    }

    pub fn is_column_visible(&self, column_key: &str) -> bool {
        self.overview_columns
            .visible
            .iter()
            .any(|key| key == column_key)
    }

    pub fn render_cell(&self, row: &DisplayRow, column_key: &str) -> Option<String> {
        let node = self.instances.get(&row.key)?;
        let column = self.column_registry.column(column_key)?;
        Some(column.render_cell(&self.row_ctx().cell(node, &row.tree_prefix)))
    }

    /// Winning row key per emphasized column, including transient records
    /// reached since the last call.
    pub(crate) fn take_emphasized_rows_by_column(
        &mut self,
        rows: &[DisplayRow],
        row_ctx: &RowCtx,
    ) -> HashMap<String, String> {
        let mut emphasized = std::mem::take(&mut self.pending_transient_emphasis);

        for column_key in self.visible_column_keys() {
            let Some(column) = self.column_registry.column(&column_key) else {
                continue;
            };
            let Some(rule) = column.emphasis() else {
                continue;
            };
            if column.emphasis_lifetime() == EmphasisLifetime::TransientRecord {
                continue;
            }

            let winner = rows
                .iter()
                .filter_map(|row| {
                    let node = self.instances.get(&row.key)?;
                    let sort_key = column.sort_key(&row_ctx.cell(node, ""));
                    (!sort_key.is_null()).then_some((row.key.as_str(), sort_key))
                })
                .reduce(|best, candidate| {
                    if rule.prefers(&candidate.1, &best.1) {
                        candidate
                    } else {
                        best
                    }
                });

            if let Some((key, _)) = winner {
                emphasized.insert(column_key, key.to_string());
            }
        }

        emphasized
    }

    fn track_transient_emphasis(&mut self, updated_key: &str) {
        let Some(node) = self.instances.get(updated_key) else {
            return;
        };
        let row_ctx = self.row_ctx();
        let ctx = row_ctx.cell(node, "");

        for column_key in &self.overview_columns.order {
            let Some(column) = self.column_registry.column(column_key) else {
                continue;
            };
            let Some(rule) = column.emphasis() else {
                continue;
            };
            if column.emphasis_lifetime() != EmphasisLifetime::TransientRecord {
                continue;
            }

            let sort_key = column.sort_key(&ctx);
            if sort_key.is_null() {
                continue;
            }
            let is_record = self
                .transient_emphasis_records
                .get(column_key)
                .is_none_or(|best| rule.prefers(&sort_key, best));
            if is_record {
                self.transient_emphasis_records
                    .insert(column_key.clone(), sort_key);
                self.pending_transient_emphasis
                    .insert(column_key.clone(), updated_key.to_string());
            }
        }
    }

    fn build_tree_rows(&self, filtered_nodes: &[&InstanceState], rows: &RowCtx) -> Vec<DisplayRow> {
        let filtered_map: HashMap<&str, &InstanceState> = filtered_nodes
            .iter()
            .map(|node| (node.key.as_str(), *node))
            .collect();

        let mut out = Vec::new();
        for group in build_tree_groups(&self.instances) {
            let mut roots: Vec<&InstanceState> = group
                .roots
                .iter()
                .filter_map(|key| filtered_map.get(key.as_str()).copied())
                .collect();
            self.sort_nodes(&mut roots, rows, |node| root_kind_rank(node.kind));
            let mut rendered = HashSet::new();
            let ctx = TreeRenderCtx {
                filtered_map: &filtered_map,
                group: &group,
                rows,
            };

            for root in roots {
                rendered.insert(root.key.as_str());
                out.push(self.to_display_row(root, ""));
                self.append_tree_children(&mut out, &ctx, &root.key, "", &mut rendered);
            }
        }

        out
    }

    fn append_tree_children<'a>(
        &self,
        out: &mut Vec<DisplayRow>,
        ctx: &TreeRenderCtx<'a>,
        parent_key: &str,
        indent: &str,
        rendered: &mut HashSet<&'a str>,
    ) {
        let mut children: Vec<&'a InstanceState> = ctx
            .group
            .children
            .get(parent_key)
            .into_iter()
            .flatten()
            .filter_map(|key| ctx.filtered_map.get(key.as_str()).copied())
            .collect();
        self.sort_nodes(&mut children, ctx.rows, |_| 0);

        let last_index = children.len().saturating_sub(1);
        for (idx, child) in children.into_iter().enumerate() {
            if !rendered.insert(child.key.as_str()) {
                continue;
            }
            let (branch, continuation) = if idx == last_index {
                ("└─ ", "   ")
            } else {
                ("├─ ", "│  ")
            };
            out.push(self.to_display_row(child, &format!("{indent}{branch}")));
            self.append_tree_children(
                out,
                ctx,
                &child.key,
                &format!("{indent}{continuation}"),
                rendered,
            );
        }
    }

    /// Sorts by `group_rank` first, then the active sort column, then address.
    /// Sort keys are computed once per node rather than per comparison.
    fn sort_nodes(
        &self,
        nodes: &mut Vec<&InstanceState>,
        rows: &RowCtx,
        group_rank: impl Fn(&InstanceState) -> u8,
    ) {
        let column = self.column_registry.column(&self.sort_by);
        let mut keyed: Vec<(u8, SortKey, &InstanceState)> = nodes
            .drain(..)
            .map(|node| {
                let key = column.map_or(SortKey::Null, |column| {
                    column.sort_key(&rows.cell(node, ""))
                });
                (group_rank(node), key, node)
            })
            .collect();
        keyed.sort_by(|(rank_a, key_a, a), (rank_b, key_b, b)| {
            rank_a
                .cmp(rank_b)
                .then_with(|| apply_direction(key_a.cmp(key_b), self.sort_direction))
                .then_with(|| a.addr.cmp(&b.addr))
        });
        nodes.extend(keyed.into_iter().map(|(_, _, node)| node));
    }

    fn to_display_row(&self, node: &InstanceState, prefix: &str) -> DisplayRow {
        DisplayRow {
            key: node.key.clone(),
            tree_prefix: prefix.to_string(),
            stale: node.is_stale(self.settings.refresh_interval),
        }
    }

    pub(crate) fn row_ctx(&self) -> RowCtx {
        RowCtx {
            cluster_labels: self.cluster_labels(),
            omit_host: self.should_omit_host_in_rendering(),
        }
    }

    /// Maps raw cluster ids to short stable labels ("1", "2", ...).
    pub(crate) fn cluster_labels(&self) -> HashMap<String, String> {
        let ordered: BTreeSet<&str> = self
            .instances
            .values()
            .map(InstanceState::cluster_key)
            .collect();

        ordered
            .into_iter()
            .enumerate()
            .map(|(idx, raw_cluster)| (raw_cluster.to_string(), (idx + 1).to_string()))
            .collect()
    }

    pub fn should_omit_host_in_rendering(&self) -> bool {
        if self.force_show_host || self.instances.is_empty() {
            return false;
        }

        let mut hosts = self
            .instances
            .values()
            .map(|instance| canonical_host(&instance.addr));
        let Some(Some(first)) = hosts.next() else {
            return false;
        };
        hosts.all(|host| host.as_deref() == Some(first.as_str()))
    }

    fn is_column_auto_hidden(&self, column_key: &str) -> bool {
        match column_key {
            "addr" => !self.show_address_column(),
            "role" => self.view_mode == ViewMode::Tree,
            _ => false,
        }
    }

    fn set_sort_column(&mut self, key: String) {
        self.sort_direction = default_sort_direction_for_column(&key);
        self.sort_by = key;
    }

    fn ensure_sort_column_visible(&mut self) {
        let visible = self.visible_column_keys();
        if visible.contains(&self.sort_by) {
            return;
        }
        if let Some(next_sort) = visible.into_iter().next() {
            self.set_sort_column(next_sort);
        }
    }
}

fn matches_filter(filter: &Matcher, node: &InstanceState) -> bool {
    node.alias
        .as_deref()
        .is_some_and(|alias| filter.matches(alias))
        || filter.matches(&node.addr)
        || node
            .cluster_id
            .as_deref()
            .is_some_and(|cluster| filter.matches(cluster))
        || node.tags.iter().any(|tag| filter.matches(tag))
}

const fn apply_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Asc => ordering,
        SortDirection::Desc => ordering.reverse(),
    }
}

fn default_sort_direction_for_column(column_key: &str) -> SortDirection {
    match column_key {
        "alias" | "addr" | "role" | "cluster" | "status" => SortDirection::Asc,
        _ => SortDirection::Desc,
    }
}

const fn root_kind_rank(kind: InstanceType) -> u8 {
    match kind {
        InstanceType::Primary => 0,
        InstanceType::Cluster => 1,
        InstanceType::Standalone => 2,
        InstanceType::Replica => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveView, AppState, AuthField, DetailTab, FilterPromptMode, OverviewModal, Scroll,
        step_index,
    };
    use crate::hotkeys::{HotkeysMetric, HotkeysStatus};
    use crate::model::{
        CommandStat, InstanceState, InstanceType, RuntimeSettings, SortDirection, SortMode,
        UiTheme, ViewMode,
    };
    use crate::registry::ColumnRegistry;
    use std::collections::HashMap;
    use std::time::Duration;

    fn settings() -> RuntimeSettings {
        RuntimeSettings {
            credential_store: None,
            refresh_interval: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(300),
            command_timeout: Duration::from_millis(500),
            concurrency_limit: 4,
            leave_killed_servers: false,
            default_view: ViewMode::Tree,
            default_sort: SortMode::Address,
            ui_theme: UiTheme::default(),
        }
    }

    fn app() -> AppState {
        AppState::new(
            settings(),
            ColumnRegistry::load(None, true, SortMode::Address),
        )
    }

    fn scroll_to(app: &mut AppState, tab: DetailTab, offset: usize) {
        let scroll = &mut app.pane_mut(tab).scroll;
        scroll.viewport(100, 1);
        scroll.scroll_by(offset.cast_signed());
        assert_eq!(scroll.offset(), offset);
    }

    fn app_with_servers() -> AppState {
        let mut app = app();
        for port in [6379, 6380, 6381] {
            let addr = format!("127.0.0.1:{port}");
            app.apply_update(InstanceState::new(addr.clone(), addr));
        }
        app
    }

    #[test]
    fn server_selection_toggles_and_overrides_focus_for_actions() {
        let mut app = app_with_servers();
        assert_eq!(app.action_target_keys(), ["127.0.0.1:6379"]);
        app.toggle_server_selection();
        app.move_selection(1);
        assert_eq!(app.action_target_keys(), ["127.0.0.1:6379"]);
        app.toggle_server_selection();
        assert_eq!(
            app.action_target_keys(),
            ["127.0.0.1:6379", "127.0.0.1:6380"]
        );
        app.toggle_server_selection();
        app.move_selection(-1);
        app.toggle_server_selection();
        assert_eq!(app.selected_server_count(), 0);
        app.move_selection(2);
        assert_eq!(app.action_target_keys(), ["127.0.0.1:6381"]);
    }

    #[test]
    fn server_selection_survives_updates_sorting_filtering_and_global_actions() {
        let mut app = app_with_servers();
        app.toggle_server_selection();
        let targets = app.action_target_keys();
        app.sort_direction = SortDirection::Desc;
        app.apply_update(InstanceState::new(targets[0].clone(), targets[0].clone()));
        app.cycle_view_mode();
        app.open_sort_picker();
        app.close_overview_modal();
        app.open_column_picker();
        app.close_overview_modal();
        app.filter = "no matches".to_string();
        assert!(app.visible_rows().is_empty());
        app.toggle_server_selection();
        assert_eq!(app.action_target_keys(), targets);
        app.open_auth_form();
        assert_eq!(app.auth_form.as_ref().unwrap().target_keys, targets);
        app.close_auth_form();
        app.open_kill_picker();
        assert_eq!(app.kill_target_keys, targets);
        app.close_overview_modal();
        app.remove_instance(&targets[0]);
        assert_eq!(app.selected_server_count(), 0);
        assert_eq!(app.action_target_keys(), Vec::<String>::new());
    }

    #[test]
    fn auth_form_captures_all_selected_targets_and_shared_credentials() {
        let mut app = app_with_servers();
        app.toggle_server_selection();
        app.move_selection(1);
        app.toggle_server_selection();
        app.open_auth_form();
        app.move_selection(1);
        app.filter = "6381".to_string();
        let form = app.auth_form.as_mut().unwrap();
        form.username = "operator".to_string();
        form.password = "secret".to_string();
        assert_eq!(
            app.take_auth_credentials(),
            Some((
                vec!["127.0.0.1:6379".to_string(), "127.0.0.1:6380".to_string()],
                Some("operator".to_string()),
                "secret".to_string(),
            ))
        );
        assert!(app.auth_form.is_none());
        assert_eq!(app.selected_server_count(), 2);
    }

    #[test]
    fn single_stop_uses_captured_target_without_extra_confirmation() {
        let mut app = app_with_servers();
        app.open_kill_picker();
        app.move_selection(1);
        let (keys, _) = app.submit_kill().expect("single target submits directly");
        assert_eq!(keys, ["127.0.0.1:6379"]);
        assert_eq!(app.overview_modal, OverviewModal::None);

        app.toggle_server_selection();
        app.move_selection(1);
        app.open_kill_picker();
        let (keys, _) = app
            .submit_kill()
            .expect("one marked target submits directly");
        assert_eq!(keys, ["127.0.0.1:6380"]);
    }

    #[test]
    fn auth_form_defaults_username_and_requires_a_password() {
        let mut app = app();
        app.apply_update(InstanceState::new(
            "127.0.0.1:6380".into(),
            "127.0.0.1:6380".into(),
        ));

        app.open_auth_form();

        let form = app.auth_form.as_mut().expect("auth form should open");
        assert_eq!(form.target_keys, ["127.0.0.1:6380"]);
        assert_eq!(form.username, "default");
        assert_eq!(form.active_field, AuthField::Username);
        assert_eq!(app.take_auth_credentials(), None);

        app.toggle_auth_field();
        app.auth_active_value_mut()
            .expect("password field should be active")
            .push_str("secret");
        assert_eq!(
            app.take_auth_credentials(),
            Some((
                vec!["127.0.0.1:6380".to_string()],
                Some("default".to_string()),
                "secret".to_string(),
            ))
        );
        assert_eq!(app.overview_modal, OverviewModal::None);
        assert!(app.auth_form.is_none());
    }

    #[test]
    fn empty_auth_username_is_submitted_as_password_only() {
        let mut app = app();
        app.apply_update(InstanceState::new("socket".into(), "socket".into()));
        app.open_auth_form();
        let form = app.auth_form.as_mut().expect("auth form should open");
        form.username.clear();
        form.password = "secret".to_string();

        let (_, username, _) = app
            .take_auth_credentials()
            .expect("password should permit submission");

        assert_eq!(username, None);
    }

    #[test]
    fn tree_view_places_replicas_below_primary() {
        let mut app = app();

        let mut replica = InstanceState::new("replica".into(), "127.0.0.1:6380".into());
        replica.kind = InstanceType::Replica;
        replica.parent_addr = Some("127.0.0.1:6379".into());

        let mut primary = InstanceState::new("primary".into(), "127.0.0.1:6379".into());
        primary.kind = InstanceType::Primary;

        app.apply_update(replica);
        app.apply_update(primary);

        let rows = app.visible_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "primary");
        assert!(
            !app.render_cell(&rows[0], "alias")
                .unwrap_or_default()
                .contains("└─")
        );
        assert_eq!(rows[1].key, "replica");
        assert!(
            app.render_cell(&rows[1], "alias")
                .unwrap_or_default()
                .starts_with("└─ ")
        );
        assert_eq!(app.render_cell(&rows[0], "role").as_deref(), Some("PRI"));
        assert_eq!(app.render_cell(&rows[1], "role").as_deref(), Some("REP"));
    }

    #[test]
    fn primary_view_hides_replicas() {
        let mut app = app();
        app.view_mode = ViewMode::Primary;

        let mut replica = InstanceState::new("replica".into(), "127.0.0.1:6380".into());
        replica.kind = InstanceType::Replica;
        replica.parent_addr = Some("127.0.0.1:6379".into());

        let mut primary = InstanceState::new("primary".into(), "127.0.0.1:6379".into());
        primary.kind = InstanceType::Primary;

        let standalone = InstanceState::new("standalone".into(), "127.0.0.1:6381".into());

        app.apply_update(replica);
        app.apply_update(primary);
        app.apply_update(standalone);

        let rows = app.visible_rows();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.key == "primary"));
        assert!(rows.iter().any(|row| row.key == "standalone"));
        assert!(!rows.iter().any(|row| row.key == "replica"));
    }

    #[test]
    fn render_cell_reads_master_replication_offset_from_info() {
        let mut app = app();

        let mut primary = InstanceState::new("primary".into(), "127.0.0.1:6379".into());
        primary.kind = InstanceType::Primary;
        primary
            .info
            .insert("master_repl_offset".into(), "8909571199".into());

        app.apply_update(primary);

        let rows = app.visible_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            app.render_cell(&rows[0], "master_repl_offset").as_deref(),
            Some("8909571199")
        );
    }

    #[test]
    fn view_mode_cycles_through_all_overview_modes() {
        assert_eq!(ViewMode::Tree.cycle(), ViewMode::Flat);
        assert_eq!(ViewMode::Flat.cycle(), ViewMode::Primary);
        assert_eq!(ViewMode::Primary.cycle(), ViewMode::Tree);
    }

    #[test]
    fn tree_view_auto_hides_type_column_from_visible_columns() {
        let app = app();

        assert!(app.is_column_visible("role"));
        assert!(!app.visible_column_keys().iter().any(|key| key == "role"));
        assert_eq!(
            app.column_auto_hidden_suffix("role"),
            Some(" (auto hidden in Tree)")
        );
    }

    #[test]
    fn maps_raw_cluster_ids_to_compact_logical_ids() {
        let mut app = app();

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.cluster_id = Some("def959b8".into());
        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.cluster_id = Some("af8898a8".into());
        let mut c = InstanceState::new("c".into(), "127.0.0.1:6381".into());
        c.cluster_id = Some("def959b8".into());

        app.apply_update(a);
        app.apply_update(b);
        app.apply_update(c);

        let rows = app.visible_rows();
        let cluster_by_key: HashMap<String, String> = rows
            .into_iter()
            .map(|row| {
                (
                    row.key.clone(),
                    app.render_cell(&row, "cluster").unwrap_or_default(),
                )
            })
            .collect();

        assert_eq!(cluster_by_key.get("a"), Some(&"2".to_string()));
        assert_eq!(cluster_by_key.get("b"), Some(&"1".to_string()));
        assert_eq!(cluster_by_key.get("c"), Some(&"2".to_string()));
    }

    #[test]
    fn hides_address_column_when_all_instance_hosts_are_equal() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "10.0.0.12:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "10.0.0.12:6380".into()));
        assert!(!app.show_address_column());
    }

    #[test]
    fn keeps_address_column_when_instance_hosts_are_mixed() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "10.0.0.12:6380".into()));
        assert!(app.show_address_column());
    }

    #[test]
    fn default_label_omits_host_when_all_hosts_match() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "127.0.0.1:6380".into()));

        let rows = app.visible_rows();
        assert_eq!(app.render_cell(&rows[0], "alias").as_deref(), Some("6379"));
        assert_eq!(app.render_cell(&rows[1], "alias").as_deref(), Some("6380"));
    }

    #[test]
    fn force_show_host_override_keeps_address_column_visible() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "127.0.0.1:6380".into()));
        app.toggle_host_rendering();

        assert!(app.show_address_column());
        let rows = app.visible_rows();
        assert_eq!(
            app.render_cell(&rows[0], "alias").as_deref(),
            Some("127.0.0.1:6379")
        );
    }

    #[test]
    fn start_filter_input_sets_mode_and_clear_behavior() {
        let mut app = app();
        app.filter = "redis".to_string();

        app.start_filter_input(FilterPromptMode::Search, false);
        assert!(app.is_filtering);
        assert_eq!(app.filter_prompt_mode, FilterPromptMode::Search);
        assert_eq!(app.filter, "redis");

        app.start_filter_input(FilterPromptMode::Filter, true);
        assert!(app.is_filtering);
        assert_eq!(app.filter_prompt_mode, FilterPromptMode::Filter);
        assert_eq!(app.filter, "");
    }

    #[test]
    fn sort_picker_uses_only_visible_columns() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "127.0.0.1:6380".into()));

        let columns = app.sortable_columns();
        assert!(columns.iter().any(|key| key == "alias"));
        assert!(!columns.iter().any(|key| key == "addr"));
        assert!(!columns.iter().any(|key| key == "role"));
    }

    #[test]
    fn applying_same_sort_column_toggles_direction() {
        let mut app = app();
        app.sort_by = "status".to_string();
        app.sort_direction = SortDirection::Asc;
        app.open_sort_picker();

        app.apply_sort_picker_selection();

        assert_eq!(app.sort_by, "status");
        assert_eq!(app.sort_direction, SortDirection::Desc);
    }

    #[test]
    fn commandstats_columns_are_independent_and_survive_detail_changes() {
        use crate::commandstats::CommandstatsColumn::{Calls, Command, Usec, UsecPerCall};

        let mut app = app_with_servers();
        let overview_order = app.available_overview_columns();
        let overview_visible = app.visible_column_keys();
        let overview_sort = (app.sort_by.clone(), app.sort_direction);
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.open_column_picker();
        assert_eq!(
            app.visible_commandstats_columns(),
            [Command, Calls, Usec, UsecPerCall]
        );

        app.move_column_picker_selection(1);
        app.toggle_selected_column_visibility();
        app.move_selected_column(2); // Hidden columns can also be reordered.
        assert_eq!(app.column_picker_index, 3);
        assert!(!app.column_picker_entries()[3].visible);
        app.toggle_selected_column_visibility();
        assert_eq!(
            app.visible_commandstats_columns(),
            [Command, Usec, UsecPerCall, Calls]
        );
        app.toggle_selected_column_visibility();
        app.close_overview_modal();
        app.close_detail_view();

        app.open_column_picker();
        assert_eq!(app.available_overview_columns(), overview_order);
        assert_eq!(app.visible_column_keys(), overview_visible);
        assert_eq!((app.sort_by.clone(), app.sort_direction), overview_sort);
        app.close_overview_modal();
        app.move_selection(1);
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.apply_update(InstanceState::new("new".into(), "127.0.0.1:6382".into()));
        app.open_column_picker();
        assert_eq!(
            app.visible_commandstats_columns(),
            [Command, Usec, UsecPerCall]
        );
        assert!(!app.column_picker_entries()[3].visible);
    }

    #[test]
    fn commandstats_discovers_metrics_without_changing_visibility_order_or_focus() {
        use crate::commandstats::CommandstatsColumn::{self, Command, Metric};

        let mut app = app();
        let mut server = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        server.detail.commandstats = crate::parse::parse_commandstats(&crate::parse::parse_info(
            "# Commandstats\ncmdstat_get:calls=3,usec=9,usec_per_call=3.00,failed_calls=2\n\
             cmdstat_set:calls=2,usec=8,usec_per_call=4.00,rejected_calls=1,failed_calls=0\n",
        ));
        app.apply_update(server.clone());
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.open_column_picker();
        assert_eq!(
            app.visible_commandstats_columns(),
            CommandstatsColumn::DEFAULT
        );
        let entries = app.column_picker_entries();
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[4].label, "failed_calls");
        assert_eq!(entries[5].label, "rejected_calls");
        assert!(!entries[4].visible);
        assert!(!entries[5].visible);

        app.move_column_picker_selection(4);
        app.toggle_selected_column_visibility();
        app.move_selected_column(-3);
        let expected = app.visible_commandstats_columns();
        assert_eq!(&expected[..2], &[Command, Metric("failed_calls".into())]);
        app.apply_update(server); // Repeated metrics are not duplicated.
        let mut other = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        other.detail.commandstats = crate::parse::parse_commandstats(&crate::parse::parse_info(
            "# Commandstats\ncmdstat_get:calls=1,usec=2,usec_per_call=2.00,a_new_metric=123\n",
        ));
        app.apply_update(other);
        assert_eq!(app.column_picker_index, 1);
        assert_eq!(app.column_picker_entries()[1].label, "failed_calls");
        assert_eq!(app.column_picker_entries().len(), 7);
        assert_eq!(app.column_picker_entries()[6].label, "a_new_metric");
        assert!(!app.column_picker_entries()[6].visible);
        assert_eq!(app.visible_commandstats_columns(), expected);

        app.remove_instance("a");
        app.close_overview_modal();
        app.open_column_picker();
        assert_eq!(app.visible_commandstats_columns(), expected);
        assert!(app.column_picker_entries()[1].visible);
        app.column_picker_index = 1;
        app.toggle_selected_column_visibility();
        assert_eq!(
            app.visible_commandstats_columns(),
            CommandstatsColumn::DEFAULT
        );
    }

    #[test]
    fn commandstats_picker_keeps_one_column_and_clamps_movement() {
        use crate::commandstats::CommandstatsColumn::UsecPerCall;

        let mut app = app();
        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Commandstats;
        app.open_column_picker();
        for index in 0..4 {
            app.column_picker_index = index;
            app.toggle_selected_column_visibility();
        }
        assert_eq!(app.visible_commandstats_columns(), [UsecPerCall]);
        app.close_overview_modal();
        app.open_column_picker();
        assert_eq!(app.column_picker_index, 3);
        app.move_selected_column(isize::MIN);
        assert_eq!(app.column_picker_index, 0);
        assert_eq!(app.column_picker_entries()[0].label, "Usec/Call");
        app.toggle_selected_column_visibility();
        assert_eq!(app.visible_commandstats_columns(), [UsecPerCall]);
        app.move_column_picker_selection(isize::MAX);
        assert_eq!(app.column_picker_index, 3);
        app.move_selected_column(isize::MAX);
        assert_eq!(app.column_picker_index, 3);
        app.move_column_picker_selection(isize::MIN);
        assert_eq!(app.column_picker_index, 0);
    }

    #[test]
    fn column_picker_uses_all_available_columns() {
        let app = app();
        let columns = app.available_overview_columns();

        assert!(columns.iter().any(|key| key == "alias"));
        assert!(columns.iter().any(|key| key == "cluster"));
    }

    #[test]
    fn available_overview_columns_keep_visible_columns_first_in_runtime_order() {
        let mut app = app();
        app.overview_columns.visible = vec!["ops".to_string(), "alias".to_string()];
        app.overview_columns.order = vec![
            "ops".to_string(),
            "alias".to_string(),
            "cluster".to_string(),
        ];

        let columns = app.available_overview_columns();

        assert_eq!(columns.first().map(String::as_str), Some("ops"));
        assert_eq!(columns.get(1).map(String::as_str), Some("alias"));
        assert!(columns.iter().any(|key| key == "cluster"));
    }

    #[test]
    fn hiding_active_sort_column_moves_sort_to_next_visible_column() {
        let mut app = app();
        app.sort_by = "ops".to_string();
        app.sort_direction = SortDirection::Desc;
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert!(!app.is_column_visible("ops"));
        assert_ne!(app.sort_by, "ops");
        assert!(
            app.visible_column_keys()
                .iter()
                .any(|key| key == &app.sort_by)
        );
    }

    #[test]
    fn column_picker_keeps_at_least_one_visible_column() {
        let mut app = app();
        app.overview_columns.visible = vec!["alias".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(app.overview_columns.visible, vec!["alias".to_string()]);
        assert_eq!(app.visible_column_keys(), vec!["alias".to_string()]);
    }

    #[test]
    fn auto_hidden_address_column_does_not_count_as_last_visible_column() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "127.0.0.1:6380".into()));
        app.overview_columns.visible = vec!["alias".to_string(), "addr".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(
            app.overview_columns.visible,
            vec!["alias".to_string(), "addr".to_string()]
        );
        assert_eq!(app.visible_column_keys(), vec!["alias".to_string()]);
    }

    #[test]
    fn auto_hidden_type_column_does_not_count_as_last_visible_column() {
        let mut app = app();
        app.overview_columns.visible = vec!["alias".to_string(), "role".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(
            app.overview_columns.visible,
            vec!["alias".to_string(), "role".to_string()]
        );
        assert_eq!(app.visible_column_keys(), vec!["alias".to_string()]);
    }

    #[test]
    fn cycling_to_tree_view_moves_sort_off_auto_hidden_type_column() {
        let mut app = app();
        app.view_mode = ViewMode::Flat;
        app.sort_by = "role".to_string();
        app.sort_direction = SortDirection::Asc;

        app.cycle_view_mode();

        assert_eq!(app.view_mode, ViewMode::Primary);
        assert_eq!(app.sort_by, "role");
        app.cycle_view_mode();
        assert_eq!(app.view_mode, ViewMode::Tree);
        assert_ne!(app.sort_by, "role");
        assert!(
            app.visible_column_keys()
                .iter()
                .any(|key| key == &app.sort_by)
        );
    }

    #[test]
    fn moving_selected_column_reorders_runtime_columns() {
        let mut app = app();
        app.overview_columns.order =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.overview_columns.visible =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.open_column_picker();
        app.column_picker_index = 1;

        app.move_selected_column(1);

        assert_eq!(
            app.overview_columns.order,
            vec!["alias".to_string(), "status".to_string(), "ops".to_string()]
        );
        assert_eq!(app.column_picker_index, 2);
    }

    #[test]
    fn moving_hidden_column_reorders_runtime_order() {
        let mut app = app();
        app.overview_columns.order = vec![
            "alias".to_string(),
            "ops".to_string(),
            "cluster".to_string(),
        ];
        app.overview_columns.visible = vec!["alias".to_string(), "ops".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "cluster")
            .unwrap_or(0);

        app.move_selected_column(-1);

        assert_eq!(
            app.overview_columns.order,
            vec![
                "alias".to_string(),
                "cluster".to_string(),
                "ops".to_string(),
            ]
        );
        assert_eq!(app.column_picker_index, 1);
    }

    #[test]
    fn toggling_column_visibility_keeps_picker_order_stable() {
        let mut app = app();
        app.overview_columns.order = vec![
            "alias".to_string(),
            "ops".to_string(),
            "cluster".to_string(),
            "status".to_string(),
        ];
        app.overview_columns.visible = vec![
            "alias".to_string(),
            "ops".to_string(),
            "cluster".to_string(),
            "status".to_string(),
        ];
        app.open_column_picker();
        app.column_picker_index = 1;
        let initial_columns = app.available_overview_columns();
        let initial_ops_index = initial_columns
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        let hidden_columns = app.available_overview_columns();
        let hidden_ops_index = hidden_columns
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);
        assert_eq!(hidden_ops_index, initial_ops_index);
        assert_eq!(
            app.visible_column_keys(),
            vec![
                "alias".to_string(),
                "cluster".to_string(),
                "status".to_string(),
            ]
        );

        app.toggle_selected_column_visibility();

        let restored_columns = app.available_overview_columns();
        let restored_ops_index = restored_columns
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);
        assert_eq!(restored_ops_index, initial_ops_index);
        assert_eq!(
            app.visible_column_keys(),
            vec![
                "alias".to_string(),
                "ops".to_string(),
                "cluster".to_string(),
                "status".to_string(),
            ]
        );
    }

    #[test]
    fn moving_column_swaps_with_hidden_neighbors_in_picker_order() {
        let mut app = app();
        app.overview_columns.order = vec![
            "alias".to_string(),
            "cluster".to_string(),
            "ops".to_string(),
            "status".to_string(),
        ];
        app.overview_columns.visible =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);

        app.move_selected_column(-1);

        assert_eq!(
            app.overview_columns.order,
            vec![
                "alias".to_string(),
                "ops".to_string(),
                "cluster".to_string(),
                "status".to_string(),
            ]
        );
        assert_eq!(
            app.visible_column_keys(),
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()]
        );
        assert_eq!(app.column_picker_index, 1);
    }

    #[test]
    fn emphasizes_max_latency_rows_per_visible_column() {
        let mut app = app();
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.last_latency_ms = Some(0.25);
        a.max_latency_ms = 1.4;

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.last_latency_ms = Some(0.95);
        b.max_latency_ms = 0.8;

        let mut c = InstanceState::new("c".into(), "127.0.0.1:6381".into());
        c.last_latency_ms = Some(0.40);
        c.max_latency_ms = 2.1;

        app.apply_update(a);
        app.apply_update(b);
        app.apply_update(c);

        let rows = app.visible_rows();
        let row_ctx = app.row_ctx();
        let emphasized = app.take_emphasized_rows_by_column(&rows, &row_ctx);

        assert_eq!(emphasized.get("lat_last"), Some(&"b".to_string()));
        assert_eq!(emphasized.get("lat_max"), Some(&"c".to_string()));
    }

    #[test]
    fn max_latency_emphasis_is_only_emitted_for_the_record_frame() {
        let mut app = app();
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.max_latency_ms = 1.4;

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.max_latency_ms = 2.1;

        app.apply_update(a);
        app.apply_update(b);

        let rows = app.visible_rows();
        let row_ctx = app.row_ctx();
        let emphasized = app.take_emphasized_rows_by_column(&rows, &row_ctx);
        assert_eq!(emphasized.get("lat_max"), Some(&"b".to_string()));

        let rows = app.visible_rows();
        let row_ctx = app.row_ctx();
        let emphasized = app.take_emphasized_rows_by_column(&rows, &row_ctx);
        assert_eq!(emphasized.get("lat_max"), None);

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.max_latency_ms = 2.6;
        app.apply_update(b);

        let rows = app.visible_rows();
        let row_ctx = app.row_ctx();
        let emphasized = app.take_emphasized_rows_by_column(&rows, &row_ctx);
        assert_eq!(emphasized.get("lat_max"), Some(&"b".to_string()));
    }

    #[test]
    fn visible_commandstats_filters_and_sorts_by_calls_desc() {
        let mut app = app();
        app.pane_mut(DetailTab::Commandstats).filter = "clu".to_string();

        let stats = vec![
            CommandStat {
                command: "get".into(),
                calls: 100,
                usec: 1_000,
                usec_per_call: 10.0,
                additional_metrics: std::collections::BTreeMap::new(),
            },
            CommandStat {
                command: "cluster|shards".into(),
                calls: 500,
                usec: 2_000,
                usec_per_call: 4.0,
                additional_metrics: std::collections::BTreeMap::new(),
            },
            CommandStat {
                command: "cluster|info".into(),
                calls: 50,
                usec: 500,
                usec_per_call: 10.0,
                additional_metrics: std::collections::BTreeMap::new(),
            },
        ];

        let visible = app.visible_commandstats(&stats);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].command, "cluster|shards");
        assert_eq!(visible[1].command, "cluster|info");
    }

    #[test]
    fn scroll_is_clamped_to_the_rendered_viewport() {
        let mut scroll = Scroll::default();
        assert_eq!(scroll.viewport(4, 3), 0..3);
        scroll.scroll_by(10);
        assert_eq!(scroll.offset(), 1);
        assert_eq!(scroll.viewport(4, 3), 1..4);
        scroll.scroll_by(-5);
        assert_eq!(scroll.offset(), 0);
        scroll.to_end();
        assert_eq!(scroll.offset(), 1);
        scroll.page_by(-1);
        assert_eq!(scroll.offset(), 0);
        assert_eq!(scroll.viewport(30, 10), 0..10);
        scroll.page_by(2);
        assert_eq!(scroll.viewport(30, 10), 20..30);
        // Content shrinking between frames pulls the offset back into range.
        assert_eq!(scroll.viewport(12, 10), 2..12);
        assert_eq!(scroll.viewport(0, 10), 0..0);
    }

    #[test]
    fn step_index_saturates_and_handles_empty_lists() {
        assert_eq!(step_index(0, -1, 3), 0);
        assert_eq!(step_index(1, 1, 3), 2);
        assert_eq!(step_index(1, isize::MAX, 3), 2);
        assert_eq!(step_index(2, isize::MIN, 3), 0);
        assert_eq!(step_index(5, 0, 0), 0);
    }

    #[test]
    fn detail_tabs_rotate_and_map_shortcuts() {
        assert_eq!(DetailTab::Summary.rotate(-1), DetailTab::Hotkeys);
        assert_eq!(DetailTab::Hotkeys.rotate(1), DetailTab::Summary);
        assert_eq!(DetailTab::Summary.rotate(1), DetailTab::InfoRaw);
        assert_eq!(DetailTab::InfoRaw.rotate(-1), DetailTab::Summary);
        assert_eq!(DetailTab::Summary.rotate(11), DetailTab::InfoRaw);
        assert_eq!(DetailTab::from_shortcut('K'), Some(DetailTab::Hotkeys));
        assert_eq!(DetailTab::from_shortcut('k'), None);
    }

    #[test]
    fn start_bigkeys_filter_input_sets_clear_behavior() {
        let mut app = app();
        app.detail_tab = DetailTab::Bigkeys;
        app.pane_mut(DetailTab::Bigkeys).filter = "session".to_string();

        app.start_active_detail_filter_input(false);
        assert!(app.pane(DetailTab::Bigkeys).is_filtering);
        assert_eq!(app.pane(DetailTab::Bigkeys).filter, "session");

        app.start_active_detail_filter_input(true);
        assert!(app.pane(DetailTab::Bigkeys).is_filtering);
        assert_eq!(app.pane(DetailTab::Bigkeys).filter, "");
        assert_eq!(app.pane(DetailTab::Bigkeys).scroll.offset(), 0);
    }

    #[test]
    fn visible_bigkeys_filters_by_key_and_type() {
        let mut app = app();
        let entries = vec![
            crate::model::BigkeyEntry {
                key: "session:1".into(),
                key_type: "string".into(),
                size: Some(32),
                memory_usage: Some(128),
            },
            crate::model::BigkeyEntry {
                key: "timeline".into(),
                key_type: "zset".into(),
                size: Some(2_000),
                memory_usage: Some(70_968),
            },
            crate::model::BigkeyEntry {
                key: "profile".into(),
                key_type: "hash".into(),
                size: Some(100),
                memory_usage: Some(2_123),
            },
        ];

        app.pane_mut(DetailTab::Bigkeys).filter = "set".to_string();
        let visible = app.visible_bigkeys(&entries);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].key, "timeline");

        app.pane_mut(DetailTab::Bigkeys).filter = "session".to_string();
        let visible = app.visible_bigkeys(&entries);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].key, "session:1");
    }

    #[test]
    fn start_detail_text_filter_input_sets_clear_behavior() {
        let mut app = app();
        app.detail_tab = DetailTab::InfoRaw;
        app.pane_mut(DetailTab::InfoRaw).filter = "run_id".to_string();

        app.start_active_detail_filter_input(false);
        assert!(app.pane(DetailTab::InfoRaw).is_filtering);
        assert_eq!(app.pane(DetailTab::InfoRaw).filter, "run_id");

        app.start_active_detail_filter_input(true);
        assert!(app.pane(DetailTab::InfoRaw).is_filtering);
        assert_eq!(app.pane(DetailTab::InfoRaw).filter, "");
        assert_eq!(app.pane(DetailTab::InfoRaw).scroll.offset(), 0);
    }

    #[test]
    fn visible_detail_text_lines_filters_case_insensitively() {
        let mut app = app();
        app.detail_tab = DetailTab::InfoRaw;
        app.pane_mut(DetailTab::InfoRaw).filter = "RUN_ID".to_string();

        let body = "# Server\nrun_id:abc123\nredis_version:8.0.0";

        let visible = app.visible_detail_text_lines(DetailTab::InfoRaw, body);
        assert_eq!(visible, vec!["run_id:abc123"]);
    }

    #[test]
    fn start_active_detail_filter_input_works_for_any_detail_tab() {
        let mut app = app();

        app.active_view = ActiveView::Detail;
        app.detail_tab = DetailTab::Summary;
        app.pane_mut(DetailTab::Summary).filter = "memory".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.pane(DetailTab::Summary).is_filtering);
        assert_eq!(app.pane(DetailTab::Summary).filter, "memory");

        app.detail_tab = DetailTab::Commandstats;
        app.pane_mut(DetailTab::Commandstats).filter = "get".to_string();
        scroll_to(&mut app, DetailTab::Commandstats, 7);
        app.start_active_detail_filter_input(true);
        assert!(app.pane(DetailTab::Commandstats).is_filtering);
        assert_eq!(app.pane(DetailTab::Commandstats).filter, "");
        assert_eq!(app.pane(DetailTab::Commandstats).scroll.offset(), 0);

        app.detail_tab = DetailTab::Bigkeys;
        app.pane_mut(DetailTab::Bigkeys).filter = "session".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.pane(DetailTab::Bigkeys).is_filtering);
        assert_eq!(app.pane(DetailTab::Bigkeys).filter, "session");

        app.detail_tab = DetailTab::Hotkeys;
        app.pane_mut(DetailTab::Hotkeys).filter = "alpha".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.pane(DetailTab::Hotkeys).is_filtering);
        assert_eq!(app.pane(DetailTab::Hotkeys).filter, "alpha");
    }

    #[test]
    fn close_detail_view_clears_all_detail_filters() {
        let mut app = app();
        app.active_view = ActiveView::Detail;
        app.pane_mut(DetailTab::Summary).filter = "sum".to_string();
        app.pane_mut(DetailTab::Summary).is_filtering = true;
        scroll_to(&mut app, DetailTab::Summary, 1);
        app.pane_mut(DetailTab::InfoRaw).filter = "raw".to_string();
        app.pane_mut(DetailTab::Commandstats).filter = "cmd".to_string();
        app.pane_mut(DetailTab::Bigkeys).filter = "key".to_string();
        app.pane_mut(DetailTab::Bigkeys).is_filtering = true;
        scroll_to(&mut app, DetailTab::Bigkeys, 3);
        app.pane_mut(DetailTab::Hotkeys).filter = "hot".to_string();
        app.pane_mut(DetailTab::Hotkeys).is_filtering = true;
        scroll_to(&mut app, DetailTab::Hotkeys, 4);

        app.close_detail_view();

        assert_eq!(app.active_view, ActiveView::Overview);
        for tab in DetailTab::ALL {
            let view = app.pane(tab);
            assert_eq!(view.filter, "");
            assert!(!view.is_filtering);
            assert_eq!(view.scroll.offset(), 0);
        }
    }

    #[test]
    fn visible_hotkeys_filters_by_key() {
        let mut app = app();
        let entries = vec![
            crate::hotkeys::HotkeyEntry {
                key: "alpha".to_string(),
                value: 10,
            },
            crate::hotkeys::HotkeyEntry {
                key: "beta".to_string(),
                value: 5,
            },
        ];

        app.pane_mut(DetailTab::Hotkeys).filter = "alp".to_string();
        let visible = app.visible_hotkeys(&entries);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].key, "alpha");
    }

    #[test]
    fn local_hotkeys_reset_masks_non_running_updates() {
        let mut app = app();
        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.detail.hotkeys.status = HotkeysStatus::Ready;
        instance.detail.hotkeys.selected_metric = Some(HotkeysMetric::Cpu);
        instance.detail.hotkeys.total_value = Some(42);
        instance.detail.hotkeys.entries = vec![crate::hotkeys::HotkeyEntry {
            key: "alpha".to_string(),
            value: 42,
        }];
        app.apply_update(instance.clone());

        app.reset_hotkeys_locally("a");

        let mut stale_update = instance;
        stale_update.detail.hotkeys.last_error = Some("stale".to_string());
        app.apply_update(stale_update);

        let hotkeys = &app
            .instances
            .get("a")
            .expect("instance should exist")
            .detail
            .hotkeys;
        assert_eq!(hotkeys.status, HotkeysStatus::Idle);
        assert!(hotkeys.selected_metric.is_none());
        assert_eq!(hotkeys.entries, Vec::new());
    }

    #[test]
    fn clearing_local_hotkeys_reset_allows_new_updates() {
        let mut app = app();
        let mut instance = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        instance.detail.hotkeys.status = HotkeysStatus::Ready;
        instance.detail.hotkeys.selected_metric = Some(HotkeysMetric::Cpu);
        app.apply_update(instance.clone());

        app.reset_hotkeys_locally("a");
        app.clear_hotkeys_local_reset("a");

        let mut running_update = instance;
        running_update
            .detail
            .hotkeys
            .start(HotkeysMetric::Net, Duration::from_secs(60));
        app.apply_update(running_update);

        let hotkeys = &app
            .instances
            .get("a")
            .expect("instance should exist")
            .detail
            .hotkeys;
        assert_eq!(hotkeys.status, HotkeysStatus::Running);
        assert_eq!(hotkeys.selected_metric, Some(HotkeysMetric::Net));
    }
}
