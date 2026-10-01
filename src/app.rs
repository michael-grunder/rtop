use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::column::{Emphasis, EmphasisLifetime, RenderCtx, SortCtx, SortKey};
use crate::commandstats::CommandstatsColumn;
use crate::discovery::{DiscoveryEvent, DiscoveryStatus, VerifiedInstance};
use crate::model::{
    InstanceState, InstanceType, KillAction, RuntimeSettings, SortDirection, ViewMode,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewModal {
    None,
    SortPicker,
    ColumnPicker,
    KillPicker,
    KillConfirmation,
    AuthForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnPickerTarget {
    Overview,
    Commandstats,
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

impl FilterPromptMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Filter => "Filter",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub key: String,
    pub tree_prefix: String,
    pub stale: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailPaneState {
    pub filter: String,
    pub is_filtering: bool,
    pub scroll_offset: usize,
}

pub type CommandstatsViewState = DetailPaneState;
pub type BigkeysViewState = DetailPaneState;
pub type HotkeysViewState = DetailPaneState;
pub type DetailTextViewState = DetailPaneState;

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
    commandstats_column_order: Vec<CommandstatsColumn>,
    visible_commandstats_columns: Vec<CommandstatsColumn>,
    pub filter: String,
    pub is_filtering: bool,
    pub filter_prompt_mode: FilterPromptMode,
    pub show_help: bool,
    pub active_view: ActiveView,
    pub previous_view: ActiveView,
    pub selected_index: usize,
    marked_keys: BTreeSet<String>,
    pub detail_tab: usize,
    pub summary_view: DetailTextViewState,
    pub latency_view: DetailTextViewState,
    pub info_raw_view: DetailTextViewState,
    pub commandstats_view: CommandstatsViewState,
    pub bigkeys_view: BigkeysViewState,
    pub hotkeys_view: HotkeysViewState,
    pub force_show_host: bool,
    pub instances: HashMap<String, InstanceState>,
    pub discovery_status: DiscoveryStatus,
    pub should_quit: bool,
    pub column_registry: ColumnRegistry,
    pub runtime_overview_column_order: Vec<String>,
    pub runtime_visible_overview: Vec<String>,
    hotkeys_locally_reset: HashSet<String>,
    pending_transient_emphasis: HashMap<String, String>,
    transient_emphasis_records: HashMap<String, SortKey>,
}

struct TreeRenderCtx<'a> {
    filtered_map: &'a HashMap<String, &'a InstanceState>,
    group: &'a TreeGroup,
    cluster_labels: &'a HashMap<String, String>,
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
            commandstats_column_order: CommandstatsColumn::DEFAULT.to_vec(),
            visible_commandstats_columns: CommandstatsColumn::DEFAULT.to_vec(),
            settings,
            filter: String::new(),
            is_filtering: false,
            filter_prompt_mode: FilterPromptMode::Filter,
            show_help: false,
            active_view: ActiveView::Overview,
            previous_view: ActiveView::Overview,
            selected_index: 0,
            marked_keys: BTreeSet::new(),
            detail_tab: 0,
            summary_view: DetailTextViewState::default(),
            latency_view: DetailTextViewState::default(),
            info_raw_view: DetailTextViewState::default(),
            commandstats_view: CommandstatsViewState::default(),
            bigkeys_view: BigkeysViewState::default(),
            hotkeys_view: HotkeysViewState::default(),
            force_show_host: false,
            instances: HashMap::new(),
            discovery_status: DiscoveryStatus::default(),
            should_quit: false,
            runtime_overview_column_order: column_registry.available_overview_columns(),
            runtime_visible_overview: column_registry.default_visible_overview_columns(),
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
            let column = CommandstatsColumn::Metric(metric.clone());
            if !self.commandstats_column_order.contains(&column) {
                self.commandstats_column_order.push(column);
            }
        }
        let key = update.key.clone();
        if self.hotkeys_locally_reset.contains(&key)
            && update.detail.hotkeys.status != crate::hotkeys::HotkeysStatus::Running
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
        let len = self.visible_rows().len();
        if len == 0 {
            self.selected_index = 0;
            return;
        }

        let current = isize::try_from(self.selected_index).unwrap_or(isize::MAX);
        let max_index = isize::try_from(len - 1).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        let next = usize::try_from(next).unwrap_or(0);
        self.selected_index = next;
    }

    pub fn clamp_selection(&mut self) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= len {
            self.selected_index = len - 1;
        }
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

    pub fn start_commandstats_filter_input(&mut self, clear_existing: bool) {
        self.start_detail_filter_input_for_tab(3, clear_existing);
    }

    pub fn visible_commandstats<'a>(
        &self,
        stats: &'a [crate::model::CommandStat],
    ) -> Vec<&'a crate::model::CommandStat> {
        let needle = self.commandstats_view.filter.trim().to_ascii_lowercase();
        let mut filtered = stats
            .iter()
            .filter(|stat| needle.is_empty() || stat.command.to_ascii_lowercase().contains(&needle))
            .collect::<Vec<_>>();
        filtered.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.command.cmp(&right.command))
        });
        filtered
    }

    pub fn clamp_commandstats_scroll(
        &mut self,
        stats: &[crate::model::CommandStat],
        page_len: usize,
    ) {
        let visible_len = self.visible_commandstats(stats).len();
        let max_offset = visible_len.saturating_sub(page_len.max(1));
        if self.commandstats_view.scroll_offset > max_offset {
            self.commandstats_view.scroll_offset = max_offset;
        }
    }

    pub fn move_commandstats_scroll(
        &mut self,
        delta: isize,
        stats: &[crate::model::CommandStat],
        page_len: usize,
    ) {
        let visible_len = self.visible_commandstats(stats).len();
        let max_offset = visible_len.saturating_sub(page_len.max(1));
        let current = isize::try_from(self.commandstats_view.scroll_offset).unwrap_or(isize::MAX);
        let max_index = isize::try_from(max_offset).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        self.commandstats_view.scroll_offset = usize::try_from(next).unwrap_or(0);
    }

    pub fn clamp_bigkeys_scroll(&mut self, rows_len: usize, page_len: usize) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        if self.bigkeys_view.scroll_offset > max_offset {
            self.bigkeys_view.scroll_offset = max_offset;
        }
    }

    pub fn move_bigkeys_scroll(&mut self, delta: isize, rows_len: usize, page_len: usize) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        let current = isize::try_from(self.bigkeys_view.scroll_offset).unwrap_or(isize::MAX);
        let max_index = isize::try_from(max_offset).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        self.bigkeys_view.scroll_offset = usize::try_from(next).unwrap_or(0);
    }

    pub const fn detail_text_view(&self, detail_tab: usize) -> Option<&DetailTextViewState> {
        match detail_tab {
            0 => Some(&self.summary_view),
            1 => Some(&self.latency_view),
            2 => Some(&self.info_raw_view),
            _ => None,
        }
    }

    pub const fn detail_text_view_mut(
        &mut self,
        detail_tab: usize,
    ) -> Option<&mut DetailTextViewState> {
        match detail_tab {
            0 => Some(&mut self.summary_view),
            1 => Some(&mut self.latency_view),
            2 => Some(&mut self.info_raw_view),
            _ => None,
        }
    }

    pub fn start_detail_text_filter_input(&mut self, clear_existing: bool) {
        self.start_detail_filter_input_for_tab(self.detail_tab, clear_existing);
    }

    pub fn visible_detail_text_lines<'a>(
        &self,
        detail_tab: usize,
        lines: &'a [String],
    ) -> Vec<&'a str> {
        let needle = self
            .detail_text_view(detail_tab)
            .map_or("", |view| view.filter.trim())
            .to_ascii_lowercase();
        lines
            .iter()
            .filter_map(|line| {
                if needle.is_empty() || line.to_ascii_lowercase().contains(&needle) {
                    Some(line.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn clamp_detail_text_scroll(
        &mut self,
        detail_tab: usize,
        rows_len: usize,
        page_len: usize,
    ) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        if let Some(view) = self.detail_text_view_mut(detail_tab)
            && view.scroll_offset > max_offset
        {
            view.scroll_offset = max_offset;
        }
    }

    pub fn move_detail_text_scroll(
        &mut self,
        detail_tab: usize,
        delta: isize,
        rows_len: usize,
        page_len: usize,
    ) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        if let Some(view) = self.detail_text_view_mut(detail_tab) {
            let current = isize::try_from(view.scroll_offset).unwrap_or(isize::MAX);
            let max_index = isize::try_from(max_offset).unwrap_or(isize::MAX);
            let next = current.saturating_add(delta).clamp(0, max_index);
            view.scroll_offset = usize::try_from(next).unwrap_or(0);
        }
    }

    pub fn start_bigkeys_filter_input(&mut self, clear_existing: bool) {
        self.start_detail_filter_input_for_tab(4, clear_existing);
    }

    pub const fn detail_pane_view(&self, detail_tab: usize) -> Option<&DetailPaneState> {
        match detail_tab {
            0 => Some(&self.summary_view),
            1 => Some(&self.latency_view),
            2 => Some(&self.info_raw_view),
            3 => Some(&self.commandstats_view),
            4 => Some(&self.bigkeys_view),
            5 => Some(&self.hotkeys_view),
            _ => None,
        }
    }

    pub const fn detail_pane_view_mut(
        &mut self,
        detail_tab: usize,
    ) -> Option<&mut DetailPaneState> {
        match detail_tab {
            0 => Some(&mut self.summary_view),
            1 => Some(&mut self.latency_view),
            2 => Some(&mut self.info_raw_view),
            3 => Some(&mut self.commandstats_view),
            4 => Some(&mut self.bigkeys_view),
            5 => Some(&mut self.hotkeys_view),
            _ => None,
        }
    }

    pub fn active_detail_view_mut(&mut self) -> Option<&mut DetailPaneState> {
        if self.active_view != ActiveView::Detail {
            return None;
        }

        self.detail_pane_view_mut(self.detail_tab)
    }

    pub fn start_active_detail_filter_input(&mut self, clear_existing: bool) {
        self.start_detail_filter_input_for_tab(self.detail_tab, clear_existing);
    }

    pub fn clear_detail_filters(&mut self) {
        for detail_tab in 0..=5 {
            if let Some(view) = self.detail_pane_view_mut(detail_tab) {
                view.filter.clear();
                view.is_filtering = false;
                view.scroll_offset = 0;
            }
        }
    }

    pub fn close_detail_view(&mut self) {
        self.clear_detail_filters();
        self.active_view = ActiveView::Overview;
    }

    fn start_detail_filter_input_for_tab(&mut self, detail_tab: usize, clear_existing: bool) {
        if let Some(view) = self.detail_pane_view_mut(detail_tab) {
            if clear_existing {
                view.filter.clear();
            }
            view.is_filtering = true;
            view.scroll_offset = 0;
        }
    }

    pub fn visible_bigkeys<'a>(
        &self,
        entries: &'a [crate::model::BigkeyEntry],
    ) -> Vec<&'a crate::model::BigkeyEntry> {
        let needle = self.bigkeys_view.filter.trim().to_ascii_lowercase();
        entries
            .iter()
            .filter(|entry| {
                needle.is_empty()
                    || entry.key.to_ascii_lowercase().contains(&needle)
                    || entry.key_type.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn clamp_hotkeys_scroll(&mut self, rows_len: usize, page_len: usize) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        if self.hotkeys_view.scroll_offset > max_offset {
            self.hotkeys_view.scroll_offset = max_offset;
        }
    }

    pub fn move_hotkeys_scroll(&mut self, delta: isize, rows_len: usize, page_len: usize) {
        let max_offset = rows_len.saturating_sub(page_len.max(1));
        let current = isize::try_from(self.hotkeys_view.scroll_offset).unwrap_or(isize::MAX);
        let max_index = isize::try_from(max_offset).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        self.hotkeys_view.scroll_offset = usize::try_from(next).unwrap_or(0);
    }

    pub fn visible_hotkeys<'a>(
        &self,
        entries: &'a [crate::hotkeys::HotkeyEntry],
    ) -> Vec<&'a crate::hotkeys::HotkeyEntry> {
        let needle = self.hotkeys_view.filter.trim().to_ascii_lowercase();
        entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.key.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn visible_rows(&self) -> Vec<DisplayRow> {
        let mut nodes: Vec<&InstanceState> = self.instances.values().collect();
        nodes.retain(|node| self.matches_filter(node));
        let cluster_labels = self.cluster_labels();
        let should_omit_host = self.should_omit_host_in_rendering();

        match self.view_mode {
            ViewMode::Tree => self.build_tree_rows(nodes, should_omit_host, &cluster_labels),
            ViewMode::Flat => {
                sort_instances(
                    &mut nodes,
                    &self.sort_by,
                    self.sort_direction,
                    &cluster_labels,
                    should_omit_host,
                    &self.column_registry,
                );
                nodes
                    .into_iter()
                    .map(|node| self.to_display_row(node, ""))
                    .collect()
            }
            ViewMode::Primary => {
                nodes.retain(|node| node.kind != InstanceType::Replica);
                sort_instances(
                    &mut nodes,
                    &self.sort_by,
                    self.sort_direction,
                    &cluster_labels,
                    should_omit_host,
                    &self.column_registry,
                );
                nodes
                    .into_iter()
                    .map(|node| self.to_display_row(node, ""))
                    .collect()
            }
        }
    }

    pub fn visible_column_keys(&self) -> Vec<String> {
        self.runtime_overview_column_order
            .iter()
            .filter(|key| {
                self.runtime_visible_overview
                    .iter()
                    .any(|visible| visible == *key)
            })
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
        self.runtime_overview_column_order
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

    pub fn sort_label(&self) -> String {
        self.column_registry.column(&self.sort_by).map_or_else(
            || self.sort_by.clone(),
            |column| column.header().to_string(),
        )
    }

    pub fn open_sort_picker(&mut self) {
        let columns = self.sortable_columns();
        self.sort_picker_index = columns
            .iter()
            .position(|key| *key == self.sort_by)
            .unwrap_or(0);
        self.overview_modal = OverviewModal::SortPicker;
    }

    pub fn open_column_picker(&mut self) {
        self.column_picker_target =
            if self.active_view == ActiveView::Detail && self.detail_tab == 3 {
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
        self.commandstats_column_order
            .iter()
            .filter(|column| self.visible_commandstats_columns.contains(column))
            .cloned()
            .collect()
    }

    pub fn column_picker_entries(&self) -> Vec<ColumnPickerEntry> {
        match self.column_picker_target {
            ColumnPickerTarget::Commandstats => self
                .commandstats_column_order
                .iter()
                .map(|column| ColumnPickerEntry {
                    label: column.header().to_string(),
                    visible: self.visible_commandstats_columns.contains(column),
                    suffix: "",
                })
                .collect(),
            ColumnPickerTarget::Overview => self
                .available_overview_columns()
                .into_iter()
                .map(|key| ColumnPickerEntry {
                    label: self
                        .column_registry
                        .column(&key)
                        .map_or_else(|| key.clone(), |column| column.header().to_string()),
                    visible: self.is_column_visible(&key),
                    suffix: self
                        .column_auto_hidden_suffix(&key)
                        .unwrap_or_else(|| if key == self.sort_by { " (sort)" } else { "" }),
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
        if let Some(mut form) = self.auth_form.take() {
            form.password.clear();
        }
        self.overview_modal = OverviewModal::None;
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
        if let Some(mut form) = self.auth_form.take() {
            form.password.clear();
        }
        self.overview_modal = OverviewModal::None;
    }

    pub fn move_sort_picker_selection(&mut self, delta: isize) {
        let columns = self.sortable_columns();
        if columns.is_empty() {
            self.sort_picker_index = 0;
            return;
        }
        let current = isize::try_from(self.sort_picker_index).unwrap_or(isize::MAX);
        let max_index = isize::try_from(columns.len() - 1).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        let next = usize::try_from(next).unwrap_or(0);
        self.sort_picker_index = next;
    }

    pub fn apply_sort_picker_selection(&mut self) {
        let columns = self.sortable_columns();
        let Some(chosen_key) = columns.get(self.sort_picker_index).cloned() else {
            self.overview_modal = OverviewModal::None;
            return;
        };
        if self.sort_by == chosen_key {
            self.sort_direction = self.sort_direction.toggle();
        } else {
            self.sort_by = chosen_key;
            self.sort_direction = default_sort_direction_for_column(&self.sort_by);
        }
        self.overview_modal = OverviewModal::None;
        self.clamp_selection();
    }

    pub fn move_column_picker_selection(&mut self, delta: isize) {
        let columns = self.column_picker_entries();
        if columns.is_empty() {
            self.column_picker_index = 0;
            return;
        }
        let current = isize::try_from(self.column_picker_index).unwrap_or(isize::MAX);
        let max_index = isize::try_from(columns.len() - 1).unwrap_or(isize::MAX);
        let next = current.saturating_add(delta).clamp(0, max_index);
        self.column_picker_index = usize::try_from(next).unwrap_or(0);
    }

    pub fn move_kill_picker_selection(&mut self, delta: isize) {
        let current = isize::try_from(self.kill_picker_index).unwrap_or(isize::MAX);
        let max_index = isize::try_from(KillAction::ALL.len().saturating_sub(1)).unwrap_or(0);
        let next = current.saturating_add(delta).clamp(0, max_index);
        self.kill_picker_index = usize::try_from(next).unwrap_or(0);
    }

    pub fn set_column_picker_reorder_mode(&mut self, enabled: bool) {
        self.column_picker_reorder_mode = enabled && self.is_column_picker_open();
    }

    pub fn move_selected_column(&mut self, delta: isize) {
        if self.column_picker_target == ColumnPickerTarget::Commandstats {
            self.column_picker_index = move_ordered_column(
                &mut self.commandstats_column_order,
                self.column_picker_index,
                delta,
            );
            return;
        }
        let columns = self.available_overview_columns();
        let Some(chosen_key) = columns.get(self.column_picker_index).cloned() else {
            return;
        };
        let Some(chosen_order_idx) = self
            .runtime_overview_column_order
            .iter()
            .position(|key| key == &chosen_key)
        else {
            return;
        };

        self.column_picker_index = move_ordered_column(
            &mut self.runtime_overview_column_order,
            chosen_order_idx,
            delta,
        );
    }

    pub fn toggle_selected_column_visibility(&mut self) {
        if self.column_picker_target == ColumnPickerTarget::Commandstats {
            let Some(column) = self.commandstats_column_order.get(self.column_picker_index) else {
                return;
            };
            if self.visible_commandstats_columns.contains(column) {
                if self.visible_commandstats_columns.len() > 1 {
                    self.visible_commandstats_columns
                        .retain(|visible| visible != column);
                }
            } else {
                self.visible_commandstats_columns.push(column.clone());
            }
            return;
        }
        let columns = self.available_overview_columns();
        let Some(chosen_key) = columns.get(self.column_picker_index).cloned() else {
            return;
        };

        if self
            .runtime_visible_overview
            .iter()
            .any(|key| key == &chosen_key)
        {
            let next_visible = self
                .runtime_visible_overview
                .iter()
                .filter(|key| key.as_str() != chosen_key)
                .filter(|key| self.column_registry.column(key).is_some())
                .filter(|key| !self.is_column_auto_hidden(key))
                .count();
            if next_visible == 0 {
                return;
            }
            self.runtime_visible_overview
                .retain(|key| key != &chosen_key);
            self.ensure_sort_column_visible();
            self.clamp_selection();
            return;
        }

        self.runtime_visible_overview.push(chosen_key);
        self.normalize_runtime_visible_columns();
        self.ensure_sort_column_visible();
        self.clamp_selection();
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
        let next_idx = (current_idx + 1) % columns.len();
        self.sort_by.clone_from(&columns[next_idx]);
        self.sort_direction = default_sort_direction_for_column(&self.sort_by);
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
        self.runtime_visible_overview
            .iter()
            .any(|key| key == column_key)
    }

    pub fn render_cell(&self, row: &DisplayRow, column_key: &str) -> Option<String> {
        let node = self.instances.get(&row.key)?;
        let column = self.column_registry.column(column_key)?;
        let cluster_labels = self.cluster_labels();
        let ctx = self.render_ctx(row, node, &cluster_labels);
        Some(column.render_cell(&ctx).text)
    }

    pub fn take_emphasized_rows_by_column(
        &mut self,
        rows: &[DisplayRow],
    ) -> HashMap<String, String> {
        let cluster_labels = self.cluster_labels();
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
                    let sort_ctx = self.sort_ctx(node, &cluster_labels);
                    let sort_key = column.sort_key(&sort_ctx);
                    if matches!(sort_key, SortKey::Null) {
                        None
                    } else {
                        Some((row.key.as_str(), sort_key))
                    }
                })
                .reduce(|best, candidate| {
                    let ordering = candidate.1.compare(&best.1);
                    let take_candidate = match rule {
                        Emphasis::Max => ordering.is_gt(),
                        Emphasis::Min => ordering.is_lt(),
                    };
                    if take_candidate { candidate } else { best }
                });

            if let Some((key, _)) = winner {
                emphasized.insert(column_key, key.to_string());
            }
        }

        emphasized
    }

    fn track_transient_emphasis(&mut self, updated_key: &str) {
        let cluster_labels = self.cluster_labels();
        let Some(node) = self.instances.get(updated_key) else {
            return;
        };

        for column_key in self.runtime_overview_column_order.clone() {
            let Some(column) = self.column_registry.column(&column_key) else {
                continue;
            };
            let Some(rule) = column.emphasis() else {
                continue;
            };
            if column.emphasis_lifetime() != EmphasisLifetime::TransientRecord {
                continue;
            }

            let sort_ctx = self.sort_ctx(node, &cluster_labels);
            let sort_key = column.sort_key(&sort_ctx);
            if matches!(sort_key, SortKey::Null) {
                continue;
            }

            let should_replace =
                self.transient_emphasis_records
                    .get(&column_key)
                    .is_none_or(|best| match rule {
                        Emphasis::Max => sort_key.compare(best).is_gt(),
                        Emphasis::Min => sort_key.compare(best).is_lt(),
                    });

            if should_replace {
                self.transient_emphasis_records
                    .insert(column_key.clone(), sort_key);
                self.pending_transient_emphasis
                    .insert(column_key, updated_key.to_string());
            }
        }
    }

    fn build_tree_rows(
        &self,
        filtered_nodes: Vec<&InstanceState>,
        should_omit_host: bool,
        cluster_labels: &HashMap<String, String>,
    ) -> Vec<DisplayRow> {
        let mut filtered_map: HashMap<String, &InstanceState> = HashMap::new();
        for node in filtered_nodes {
            filtered_map.insert(node.key.clone(), node);
        }

        let mut out = Vec::new();
        for group in build_tree_groups(&self.instances) {
            let mut roots: Vec<&InstanceState> = group
                .roots
                .iter()
                .filter_map(|key| filtered_map.get(key))
                .copied()
                .collect();
            sort_tree_roots(
                &mut roots,
                &self.sort_by,
                self.sort_direction,
                cluster_labels,
                should_omit_host,
                &self.column_registry,
            );
            let mut rendered = HashSet::new();
            let ctx = TreeRenderCtx {
                filtered_map: &filtered_map,
                group: &group,
                cluster_labels,
            };

            for root in roots {
                rendered.insert(root.key.clone());
                out.push(self.to_display_row(root, ""));
                self.append_tree_children(
                    &mut out,
                    &ctx,
                    &root.key,
                    "",
                    should_omit_host,
                    &mut rendered,
                );
            }
        }

        out
    }

    fn append_tree_children(
        &self,
        out: &mut Vec<DisplayRow>,
        ctx: &TreeRenderCtx<'_>,
        parent_key: &str,
        indent: &str,
        should_omit_host: bool,
        rendered: &mut HashSet<String>,
    ) {
        let mut children: Vec<&InstanceState> = ctx
            .group
            .children
            .get(parent_key)
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| ctx.filtered_map.get(key))
                    .copied()
                    .collect::<Vec<&InstanceState>>()
            })
            .unwrap_or_default();
        sort_instances(
            &mut children,
            &self.sort_by,
            self.sort_direction,
            ctx.cluster_labels,
            should_omit_host,
            &self.column_registry,
        );

        for (idx, child) in children.iter().enumerate() {
            if rendered.contains(&child.key) {
                continue;
            }
            rendered.insert(child.key.clone());

            let is_last = idx + 1 == children.len();
            let branch = if is_last { "└─ " } else { "├─ " };
            out.push(self.to_display_row(child, &format!("{indent}{branch}")));

            let next_indent = if is_last {
                format!("{indent}   ")
            } else {
                format!("{indent}│  ")
            };
            self.append_tree_children(
                out,
                ctx,
                &child.key,
                &next_indent,
                should_omit_host,
                rendered,
            );
        }
    }

    fn to_display_row(&self, node: &InstanceState, prefix: &str) -> DisplayRow {
        DisplayRow {
            key: node.key.clone(),
            tree_prefix: prefix.to_string(),
            stale: node.is_stale(self.settings.refresh_interval),
        }
    }

    fn matches_filter(&self, node: &InstanceState) -> bool {
        if self.filter.trim().is_empty() {
            return true;
        }
        let needle = self.filter.to_ascii_lowercase();
        node.alias
            .as_deref()
            .is_some_and(|s| s.to_ascii_lowercase().contains(&needle))
            || node.addr.to_ascii_lowercase().contains(&needle)
            || node
                .cluster_id
                .as_deref()
                .is_some_and(|s| s.to_ascii_lowercase().contains(&needle))
            || node
                .tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(&needle))
    }

    pub(crate) fn cluster_labels(&self) -> HashMap<String, String> {
        let mut ordered = BTreeSet::<String>::new();
        for instance in self.instances.values() {
            let raw_cluster = instance
                .cluster_id
                .clone()
                .unwrap_or_else(|| "Standalone".to_string());
            ordered.insert(raw_cluster);
        }

        ordered
            .into_iter()
            .enumerate()
            .map(|(idx, raw_cluster)| (raw_cluster, (idx + 1).to_string()))
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

    fn render_ctx<'a>(
        &'a self,
        row: &'a DisplayRow,
        node: &'a InstanceState,
        cluster_labels: &'a HashMap<String, String>,
    ) -> RenderCtx<'a> {
        let raw_cluster = node
            .cluster_id
            .clone()
            .unwrap_or_else(|| "Standalone".to_string());
        let cluster_label = cluster_labels.get(&raw_cluster).map(String::as_str);
        RenderCtx {
            snap: node,
            omit_host: self.should_omit_host_in_rendering(),
            tree_prefix: &row.tree_prefix,
            cluster_label,
        }
    }

    fn sort_ctx<'a>(
        &'a self,
        node: &'a InstanceState,
        cluster_labels: &'a HashMap<String, String>,
    ) -> SortCtx<'a> {
        let raw_cluster = node
            .cluster_id
            .clone()
            .unwrap_or_else(|| "Standalone".to_string());
        SortCtx {
            snap: node,
            omit_host: self.should_omit_host_in_rendering(),
            cluster_label: cluster_labels.get(&raw_cluster).map(String::as_str),
        }
    }

    fn normalize_runtime_visible_columns(&mut self) {
        let registry_columns = self.column_registry.available_overview_columns();
        let mut ordered = Vec::with_capacity(registry_columns.len());
        for key in &self.runtime_overview_column_order {
            if registry_columns.iter().any(|candidate| candidate == key)
                && !ordered.iter().any(|existing| existing == key)
            {
                ordered.push(key.clone());
            }
        }
        for key in &registry_columns {
            if !ordered.iter().any(|existing| existing == key) {
                ordered.push(key.clone());
            }
        }
        self.runtime_overview_column_order = ordered;

        let mut deduped = Vec::with_capacity(self.runtime_visible_overview.len());
        for key in &self.runtime_visible_overview {
            if self
                .runtime_overview_column_order
                .iter()
                .any(|candidate| candidate == key)
                && !deduped.iter().any(|existing| existing == key)
            {
                deduped.push(key.clone());
            }
        }
        self.runtime_visible_overview = deduped;
    }

    fn is_column_auto_hidden(&self, column_key: &str) -> bool {
        match column_key {
            "addr" => !self.show_address_column(),
            "role" => self.view_mode == ViewMode::Tree,
            _ => false,
        }
    }

    fn ensure_sort_column_visible(&mut self) {
        if self
            .visible_column_keys()
            .iter()
            .any(|key| key == &self.sort_by)
        {
            return;
        }

        if let Some(next_sort) = self.visible_column_keys().into_iter().next() {
            self.sort_by = next_sort;
            self.sort_direction = default_sort_direction_for_column(&self.sort_by);
        }
    }
}

fn move_ordered_column<T>(columns: &mut Vec<T>, index: usize, delta: isize) -> usize {
    if index >= columns.len() {
        return index;
    }
    let current = isize::try_from(index).unwrap_or(isize::MAX);
    let max_index = isize::try_from(columns.len() - 1).unwrap_or(isize::MAX);
    let next = usize::try_from(current.saturating_add(delta).clamp(0, max_index)).unwrap_or(index);
    if next != index {
        let column = columns.remove(index);
        columns.insert(next, column);
    }
    next
}

fn sort_instances(
    instances: &mut Vec<&InstanceState>,
    sort_by: &str,
    direction: SortDirection,
    cluster_labels: &HashMap<String, String>,
    omit_host: bool,
    registry: &ColumnRegistry,
) {
    instances.sort_by(|a, b| {
        compare_instances(
            a,
            b,
            sort_by,
            direction,
            cluster_labels,
            omit_host,
            registry,
        )
    });
}

fn compare_instances(
    a: &InstanceState,
    b: &InstanceState,
    sort_by: &str,
    direction: SortDirection,
    cluster_labels: &HashMap<String, String>,
    omit_host: bool,
    registry: &ColumnRegistry,
) -> Ordering {
    let ordering = registry.column(sort_by).map_or_else(
        || a.addr.cmp(&b.addr),
        |column| {
            let a_cluster = a
                .cluster_id
                .clone()
                .unwrap_or_else(|| "Standalone".to_string());
            let b_cluster = b
                .cluster_id
                .clone()
                .unwrap_or_else(|| "Standalone".to_string());
            let a_ctx = SortCtx {
                snap: a,
                omit_host,
                cluster_label: cluster_labels.get(&a_cluster).map(String::as_str),
            };
            let b_ctx = SortCtx {
                snap: b,
                omit_host,
                cluster_label: cluster_labels.get(&b_cluster).map(String::as_str),
            };
            column.sort_key(&a_ctx).compare(&column.sort_key(&b_ctx))
        },
    );

    apply_direction(ordering, direction).then_with(|| a.addr.cmp(&b.addr))
}

fn sort_tree_roots(
    instances: &mut Vec<&InstanceState>,
    sort_by: &str,
    direction: SortDirection,
    cluster_labels: &HashMap<String, String>,
    omit_host: bool,
    registry: &ColumnRegistry,
) {
    instances.sort_by(|a, b| {
        root_kind_rank(a.kind)
            .cmp(&root_kind_rank(b.kind))
            .then_with(|| {
                compare_instances(
                    a,
                    b,
                    sort_by,
                    direction,
                    cluster_labels,
                    omit_host,
                    registry,
                )
            })
    });
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
    use super::{ActiveView, AppState, AuthField, FilterPromptMode, OverviewModal};
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
        app.detail_tab = 3;
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
        app.detail_tab = 3;
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
        app.detail_tab = 3;
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
        app.detail_tab = 3;
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
        app.runtime_visible_overview = vec!["ops".to_string(), "alias".to_string()];
        app.runtime_overview_column_order = vec![
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
        app.runtime_visible_overview = vec!["alias".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(app.runtime_visible_overview, vec!["alias".to_string()]);
        assert_eq!(app.visible_column_keys(), vec!["alias".to_string()]);
    }

    #[test]
    fn auto_hidden_address_column_does_not_count_as_last_visible_column() {
        let mut app = app();
        app.apply_update(InstanceState::new("a".into(), "127.0.0.1:6379".into()));
        app.apply_update(InstanceState::new("b".into(), "127.0.0.1:6380".into()));
        app.runtime_visible_overview = vec!["alias".to_string(), "addr".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(
            app.runtime_visible_overview,
            vec!["alias".to_string(), "addr".to_string()]
        );
        assert_eq!(app.visible_column_keys(), vec!["alias".to_string()]);
    }

    #[test]
    fn auto_hidden_type_column_does_not_count_as_last_visible_column() {
        let mut app = app();
        app.runtime_visible_overview = vec!["alias".to_string(), "role".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "alias")
            .unwrap_or(0);

        app.toggle_selected_column_visibility();

        assert_eq!(
            app.runtime_visible_overview,
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
        app.runtime_overview_column_order =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.runtime_visible_overview =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.open_column_picker();
        app.column_picker_index = 1;

        app.move_selected_column(1);

        assert_eq!(
            app.runtime_overview_column_order,
            vec!["alias".to_string(), "status".to_string(), "ops".to_string()]
        );
        assert_eq!(app.column_picker_index, 2);
    }

    #[test]
    fn moving_hidden_column_reorders_runtime_order() {
        let mut app = app();
        app.runtime_overview_column_order = vec![
            "alias".to_string(),
            "ops".to_string(),
            "cluster".to_string(),
        ];
        app.runtime_visible_overview = vec!["alias".to_string(), "ops".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "cluster")
            .unwrap_or(0);

        app.move_selected_column(-1);

        assert_eq!(
            app.runtime_overview_column_order,
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
        app.runtime_overview_column_order = vec![
            "alias".to_string(),
            "ops".to_string(),
            "cluster".to_string(),
            "status".to_string(),
        ];
        app.runtime_visible_overview = vec![
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
        app.runtime_overview_column_order = vec![
            "alias".to_string(),
            "cluster".to_string(),
            "ops".to_string(),
            "status".to_string(),
        ];
        app.runtime_visible_overview =
            vec!["alias".to_string(), "ops".to_string(), "status".to_string()];
        app.open_column_picker();
        app.column_picker_index = app
            .available_overview_columns()
            .iter()
            .position(|key| key == "ops")
            .unwrap_or(0);

        app.move_selected_column(-1);

        assert_eq!(
            app.runtime_overview_column_order,
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
        let emphasized = app.take_emphasized_rows_by_column(&rows);

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
        let emphasized = app.take_emphasized_rows_by_column(&rows);
        assert_eq!(emphasized.get("lat_max"), Some(&"b".to_string()));

        let rows = app.visible_rows();
        let emphasized = app.take_emphasized_rows_by_column(&rows);
        assert_eq!(emphasized.get("lat_max"), None);

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.max_latency_ms = 2.6;
        app.apply_update(b);

        let rows = app.visible_rows();
        let emphasized = app.take_emphasized_rows_by_column(&rows);
        assert_eq!(emphasized.get("lat_max"), Some(&"b".to_string()));
    }

    #[test]
    fn visible_commandstats_filters_and_sorts_by_calls_desc() {
        let mut app = app();
        app.commandstats_view.filter = "clu".to_string();

        let stats = vec![
            CommandStat {
                command: "get".into(),
                calls: 100,
                usec: 1_000,
                usec_per_call: 10.0,
                additional_metrics: Default::default(),
            },
            CommandStat {
                command: "cluster|shards".into(),
                calls: 500,
                usec: 2_000,
                usec_per_call: 4.0,
                additional_metrics: Default::default(),
            },
            CommandStat {
                command: "cluster|info".into(),
                calls: 50,
                usec: 500,
                usec_per_call: 10.0,
                additional_metrics: Default::default(),
            },
        ];

        let visible = app.visible_commandstats(&stats);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].command, "cluster|shards");
        assert_eq!(visible[1].command, "cluster|info");
    }

    #[test]
    fn commandstats_scroll_is_clamped_to_visible_page() {
        let mut app = app();
        app.commandstats_view.scroll_offset = 10;

        let stats = vec![
            CommandStat {
                command: "a".into(),
                calls: 4,
                usec: 4,
                usec_per_call: 1.0,
                additional_metrics: Default::default(),
            },
            CommandStat {
                command: "b".into(),
                calls: 3,
                usec: 3,
                usec_per_call: 1.0,
                additional_metrics: Default::default(),
            },
            CommandStat {
                command: "c".into(),
                calls: 2,
                usec: 2,
                usec_per_call: 1.0,
                additional_metrics: Default::default(),
            },
            CommandStat {
                command: "d".into(),
                calls: 1,
                usec: 1,
                usec_per_call: 1.0,
                additional_metrics: Default::default(),
            },
        ];

        app.clamp_commandstats_scroll(&stats, 3);
        assert_eq!(app.commandstats_view.scroll_offset, 1);

        app.move_commandstats_scroll(-5, &stats, 3);
        assert_eq!(app.commandstats_view.scroll_offset, 0);
    }

    #[test]
    fn start_bigkeys_filter_input_sets_clear_behavior() {
        let mut app = app();
        app.bigkeys_view.filter = "session".to_string();

        app.start_bigkeys_filter_input(false);
        assert!(app.bigkeys_view.is_filtering);
        assert_eq!(app.bigkeys_view.filter, "session");

        app.start_bigkeys_filter_input(true);
        assert!(app.bigkeys_view.is_filtering);
        assert_eq!(app.bigkeys_view.filter, "");
        assert_eq!(app.bigkeys_view.scroll_offset, 0);
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

        app.bigkeys_view.filter = "set".to_string();
        let visible = app.visible_bigkeys(&entries);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].key, "timeline");

        app.bigkeys_view.filter = "session".to_string();
        let visible = app.visible_bigkeys(&entries);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].key, "session:1");
    }

    #[test]
    fn start_detail_text_filter_input_sets_clear_behavior() {
        let mut app = app();
        app.detail_tab = 2;
        app.info_raw_view.filter = "run_id".to_string();

        app.start_detail_text_filter_input(false);
        assert!(app.info_raw_view.is_filtering);
        assert_eq!(app.info_raw_view.filter, "run_id");

        app.start_detail_text_filter_input(true);
        assert!(app.info_raw_view.is_filtering);
        assert_eq!(app.info_raw_view.filter, "");
        assert_eq!(app.info_raw_view.scroll_offset, 0);
    }

    #[test]
    fn visible_detail_text_lines_filters_case_insensitively() {
        let mut app = app();
        app.detail_tab = 2;
        app.info_raw_view.filter = "RUN_ID".to_string();

        let lines = vec![
            "# Server".to_string(),
            "run_id:abc123".to_string(),
            "redis_version:8.0.0".to_string(),
        ];

        let visible = app.visible_detail_text_lines(2, &lines);
        assert_eq!(visible, vec!["run_id:abc123"]);
    }

    #[test]
    fn detail_text_scroll_is_clamped_to_visible_page() {
        let mut app = app();
        app.detail_tab = 0;
        app.summary_view.scroll_offset = 10;

        app.clamp_detail_text_scroll(0, 4, 3);
        assert_eq!(app.summary_view.scroll_offset, 1);

        app.move_detail_text_scroll(0, -5, 4, 3);
        assert_eq!(app.summary_view.scroll_offset, 0);
    }

    #[test]
    fn start_active_detail_filter_input_works_for_any_detail_tab() {
        let mut app = app();

        app.active_view = ActiveView::Detail;
        app.detail_tab = 0;
        app.summary_view.filter = "memory".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.summary_view.is_filtering);
        assert_eq!(app.summary_view.filter, "memory");

        app.detail_tab = 3;
        app.commandstats_view.filter = "get".to_string();
        app.commandstats_view.scroll_offset = 7;
        app.start_active_detail_filter_input(true);
        assert!(app.commandstats_view.is_filtering);
        assert_eq!(app.commandstats_view.filter, "");
        assert_eq!(app.commandstats_view.scroll_offset, 0);

        app.detail_tab = 4;
        app.bigkeys_view.filter = "session".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.bigkeys_view.is_filtering);
        assert_eq!(app.bigkeys_view.filter, "session");

        app.detail_tab = 5;
        app.hotkeys_view.filter = "alpha".to_string();
        app.start_active_detail_filter_input(false);
        assert!(app.hotkeys_view.is_filtering);
        assert_eq!(app.hotkeys_view.filter, "alpha");
    }

    #[test]
    fn close_detail_view_clears_all_detail_filters() {
        let mut app = app();
        app.active_view = ActiveView::Detail;
        app.summary_view.filter = "sum".to_string();
        app.summary_view.is_filtering = true;
        app.summary_view.scroll_offset = 1;
        app.latency_view.filter = "lat".to_string();
        app.info_raw_view.filter = "raw".to_string();
        app.commandstats_view.filter = "cmd".to_string();
        app.bigkeys_view.filter = "key".to_string();
        app.bigkeys_view.is_filtering = true;
        app.bigkeys_view.scroll_offset = 3;
        app.hotkeys_view.filter = "hot".to_string();
        app.hotkeys_view.is_filtering = true;
        app.hotkeys_view.scroll_offset = 4;

        app.close_detail_view();

        assert_eq!(app.active_view, ActiveView::Overview);
        for detail_tab in 0..=5 {
            let view = app
                .detail_pane_view(detail_tab)
                .expect("detail pane should exist");
            assert_eq!(view.filter, "");
            assert!(!view.is_filtering);
            assert_eq!(view.scroll_offset, 0);
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

        app.hotkeys_view.filter = "alp".to_string();
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
