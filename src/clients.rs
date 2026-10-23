//! CLIENT LIST data and session-local selection, independent of table layout.
use std::collections::{BTreeMap, BTreeSet};

use crate::app::{ColumnSet, Matcher, step_index};

const DEFAULT_COLUMNS: [&str; 10] = [
    "id", "addr", "name", "user", "db", "flags", "age", "idle", "cmd", "tot-mem",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRow {
    pub id: u64,
    pub fields: BTreeMap<String, String>,
    search_text: String,
}

pub fn parse_list(text: &str) -> Result<Vec<ClientRow>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: BTreeMap<String, String> = line
                .split_ascii_whitespace()
                .map(|field| {
                    field
                        .split_once('=')
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .ok_or_else(|| format!("Invalid CLIENT LIST field: {field}"))
                })
                .collect::<Result<_, _>>()?;
            let id = fields
                .get("id")
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| "CLIENT LIST row has no valid id".to_owned())?;
            Ok(ClientRow {
                id,
                fields,
                search_text: line.to_owned(),
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ClientSnapshot {
    pub rows: Vec<ClientRow>,
    pub own_id: u64,
}

pub struct ClientPane {
    pub target: Option<String>,
    pub rows: Vec<ClientRow>,
    pub own_id: Option<u64>,
    pub marked: BTreeSet<u64>,
    pub focus: Option<u64>,
    pub page_len: usize,
    pub columns: ColumnSet<String>,
    pub error: Option<String>,
    pub confirmation: Option<(String, Vec<u64>)>,
    pub result: String,
}

impl Default for ClientPane {
    fn default() -> Self {
        let columns: Vec<_> = DEFAULT_COLUMNS
            .iter()
            .map(|key| (*key).to_owned())
            .collect();
        Self {
            target: None,
            rows: Vec::new(),
            own_id: None,
            marked: BTreeSet::new(),
            focus: None,
            page_len: 1,
            columns: ColumnSet::new(columns.clone(), columns),
            error: None,
            confirmation: None,
            result: String::new(),
        }
    }
}

impl ClientPane {
    pub fn set_target(&mut self, target: Option<String>) {
        if self.target != target {
            self.target = target;
            self.rows.clear();
            self.marked.clear();
            self.focus = None;
            self.own_id = None;
            self.error = None;
        }
    }

    pub fn apply(&mut self, snapshot: Result<ClientSnapshot, String>, filter: &str) {
        match snapshot {
            Ok(snapshot) => {
                self.rows = snapshot.rows;
                self.own_id = Some(snapshot.own_id);
                self.error = None;
                let ids: BTreeSet<_> = self.rows.iter().map(|row| row.id).collect();
                self.marked.retain(|id| ids.contains(id));
                let fields: BTreeSet<_> =
                    self.rows.iter().flat_map(|row| row.fields.keys()).collect();
                for key in fields {
                    self.columns.discover(key.clone());
                }
                self.move_focus(filter, 0);
            }
            Err(error) => {
                self.error = Some(error);
                // Stale rows must not remain actionable after a failed refresh.
                self.rows.clear();
                self.marked.clear();
                self.focus = None;
            }
        }
    }

    pub fn visible(&self, filter: &str) -> Vec<&ClientRow> {
        let matcher = Matcher::new(filter);
        self.rows
            .iter()
            .filter(|row| matcher.matches(&row.search_text))
            .collect()
    }

    pub fn move_focus(&mut self, filter: &str, delta: isize) {
        let rows = self.visible(filter);
        let index = rows
            .iter()
            .position(|row| Some(row.id) == self.focus)
            .unwrap_or(0);
        self.focus = rows
            .get(step_index(index, delta, rows.len()))
            .map(|row| row.id);
    }

    pub fn toggle(&mut self, filter: &str) -> Option<(u64, bool)> {
        self.move_focus(filter, 0);
        let id = self.focus?;
        let selected = self.marked.insert(id);
        if !selected {
            self.marked.remove(&id);
        }
        Some((id, selected))
    }

    pub fn select_all(&mut self, selected: bool) {
        for row in &self.rows {
            if Some(row.id) != self.own_id {
                if selected {
                    self.marked.insert(row.id);
                } else {
                    self.marked.remove(&row.id);
                }
            }
        }
        // Bulk selection never implicitly opts into killing our own connection.
        if let Some(id) = self.own_id {
            self.marked.remove(&id);
        }
    }

    pub fn ask_kill(&mut self, filter: &str) {
        self.move_focus(filter, 0);
        let ids = if self.marked.is_empty() {
            self.focus.into_iter().collect::<Vec<_>>()
        } else {
            self.marked.iter().copied().collect()
        };
        if !ids.is_empty()
            && let Some(target) = &self.target
        {
            self.confirmation = Some((target.clone(), ids));
        }
    }

    pub fn kill_prompt(&self) -> String {
        match self.confirmation.as_ref().map(|(_, ids)| ids.as_slice()) {
            Some([id]) => format!("Kill client {id}?"),
            Some(ids) => format!("Kill {} selected clients?", ids.len()),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_and_future_fields_and_rejects_missing_ids() {
        let rows = parse_list("id=1 name= user=default future=a=b\n").unwrap();
        assert_eq!(rows[0].fields["name"], "");
        assert_eq!(rows[0].fields["future"], "a=b");
        assert!(parse_list("addr=localhost").is_err());
        assert!(parse_list("id=oops").is_err());
        assert_eq!(parse_list("").unwrap(), Vec::<ClientRow>::new());
    }

    #[test]
    fn selection_is_stable_and_bulk_skips_self_but_explicit_selection_allows_it() {
        let mut pane = ClientPane::default();
        pane.set_target(Some("server".into()));
        pane.apply(
            Ok(ClientSnapshot {
                rows: parse_list("id=1 name=alpha extra=needle\nid=2 name=beta\nid=3 name=self")
                    .unwrap(),
                own_id: 3,
            }),
            "",
        );
        assert_eq!(pane.visible("ndl").len(), 1);
        assert_eq!(pane.visible("'ndl").len(), 0);
        assert!(pane.columns.order.contains(&"extra".to_owned()));
        pane.ask_kill("");
        assert_eq!(pane.kill_prompt(), "Kill client 1?");
        pane.select_all(true);
        assert_eq!(pane.marked, BTreeSet::from([1, 2]));
        pane.ask_kill("self");
        assert_eq!(pane.kill_prompt(), "Kill 2 selected clients?");
        pane.toggle("self");
        assert_eq!(pane.marked, BTreeSet::from([1, 2, 3]));
        pane.rows.reverse();
        pane.move_focus("", 0);
        assert_eq!(pane.focus, Some(3));
        pane.select_all(false);
        assert!(pane.marked.is_empty());
        pane.set_target(Some("other".into()));
        assert_eq!(pane.rows, Vec::<ClientRow>::new());
        assert_eq!(pane.focus, None);
    }
}
