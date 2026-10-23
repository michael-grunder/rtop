//! CLIENT LIST data and session-local selection, independent of table layout.
use std::collections::{BTreeMap, BTreeSet};

use crate::app::{ColumnSet, Matcher, step_index};

const DEFAULT_COLUMNS: [&str; 10] = [
    "id", "addr", "name", "user", "db", "flags", "age", "idle", "cmd", "tot-mem",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientKey {
    pub server: String,
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRow {
    pub server: String,
    pub id: u64,
    pub fields: BTreeMap<String, String>,
    search_text: String,
}

impl ClientRow {
    pub fn key(&self) -> ClientKey {
        ClientKey {
            server: self.server.clone(),
            id: self.id,
        }
    }
}

pub fn parse_list(server: &str, text: &str) -> Result<Vec<ClientRow>, String> {
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
                server: server.to_owned(),
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
    pub targets: BTreeMap<String, String>,
    pub rows: Vec<ClientRow>,
    pub own_ids: BTreeMap<String, u64>,
    pub marked: BTreeSet<ClientKey>,
    pub focus: Option<ClientKey>,
    pub page_len: usize,
    pub columns: ColumnSet<String>,
    pub errors: BTreeMap<String, String>,
    pub confirmation: Option<BTreeMap<String, Vec<u64>>>,
    pub result: String,
}

impl Default for ClientPane {
    fn default() -> Self {
        let columns: Vec<_> = DEFAULT_COLUMNS
            .iter()
            .map(|key| (*key).to_owned())
            .collect();
        Self {
            targets: BTreeMap::new(),
            rows: Vec::new(),
            own_ids: BTreeMap::new(),
            marked: BTreeSet::new(),
            focus: None,
            page_len: 1,
            columns: ColumnSet::new(columns.clone(), columns),
            errors: BTreeMap::new(),
            confirmation: None,
            result: String::new(),
        }
    }
}

impl ClientPane {
    pub fn set_targets(&mut self, targets: BTreeMap<String, String>) {
        if !self.targets.keys().eq(targets.keys()) {
            self.rows.clear();
            self.marked.clear();
            self.focus = None;
            self.own_ids.clear();
            self.errors.clear();
        }
        self.targets = targets;
    }

    pub fn apply(&mut self, server: &str, snapshot: Result<ClientSnapshot, String>, filter: &str) {
        if !self.targets.contains_key(server) {
            return;
        }
        self.rows.retain(|row| row.server != server);
        match snapshot {
            Ok(snapshot) => {
                self.rows.extend(snapshot.rows);
                self.rows
                    .sort_by(|a, b| (&a.server, a.id).cmp(&(&b.server, b.id)));
                self.own_ids.insert(server.to_owned(), snapshot.own_id);
                self.errors.remove(server);
                let fields: BTreeSet<_> =
                    self.rows.iter().flat_map(|row| row.fields.keys()).collect();
                for key in fields {
                    self.columns.discover(key.clone());
                }
            }
            Err(error) => {
                self.errors.insert(server.to_owned(), error);
                self.own_ids.remove(server);
            }
        }
        // Discard only stale clients, preserving healthy servers' selections.
        let ids: BTreeSet<_> = self.rows.iter().map(ClientRow::key).collect();
        self.marked.retain(|key| ids.contains(key));
        self.move_focus(filter, 0);
    }

    pub fn is_own(&self, row: &ClientRow) -> bool {
        self.own_ids.get(&row.server) == Some(&row.id)
    }

    pub fn alias(&self, server: &str) -> &str {
        self.targets.get(server).map_or("-", String::as_str)
    }

    pub fn visible(&self, filter: &str) -> Vec<&ClientRow> {
        let matcher = Matcher::new(filter);
        self.rows
            .iter()
            .filter(|row| {
                matcher.matches(&format!(
                    "{} {} {}",
                    self.alias(&row.server),
                    row.server,
                    row.search_text
                ))
            })
            .collect()
    }

    pub fn move_focus(&mut self, filter: &str, delta: isize) {
        let rows = self.visible(filter);
        let index = rows
            .iter()
            .position(|row| self.focus.as_ref() == Some(&row.key()))
            .unwrap_or(0);
        self.focus = rows
            .get(step_index(index, delta, rows.len()))
            .map(|row| row.key());
    }

    pub fn toggle(&mut self, filter: &str) -> Option<(ClientKey, bool)> {
        self.move_focus(filter, 0);
        let id = self.focus.clone()?;
        let selected = self.marked.insert(id.clone());
        if !selected {
            self.marked.remove(&id);
        }
        Some((id, selected))
    }

    pub fn select_all(&mut self, selected: bool) {
        for row in &self.rows {
            let key = row.key();
            if selected && !self.is_own(row) {
                self.marked.insert(key);
            } else {
                self.marked.remove(&key);
            }
        }
    }

    pub fn ask_kill(&mut self, filter: &str) {
        self.move_focus(filter, 0);
        let clients = if self.marked.is_empty() {
            self.focus.iter().cloned().collect::<Vec<_>>()
        } else {
            self.marked.iter().cloned().collect()
        };
        let mut targets: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for client in clients {
            targets.entry(client.server).or_default().push(client.id);
        }
        self.confirmation = (!targets.is_empty()).then_some(targets);
    }

    pub fn kill_prompt(&self) -> String {
        let Some(targets) = &self.confirmation else {
            return String::new();
        };
        let ids: Vec<_> = targets.values().flatten().collect();
        match ids.as_slice() {
            [id] => format!("Kill client {id}?"),
            _ if targets.len() > 1 => format!(
                "Kill {} selected clients across {} servers?",
                ids.len(),
                targets.len()
            ),
            _ => format!("Kill {} selected clients?", ids.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: u64) -> ClientKey {
        ClientKey {
            server: "server".into(),
            id,
        }
    }

    #[test]
    fn parses_empty_and_future_fields_and_rejects_missing_ids() {
        let rows = parse_list("server", "id=1 name= user=default future=a=b\n").unwrap();
        assert_eq!(rows[0].fields["name"], "");
        assert_eq!(rows[0].fields["future"], "a=b");
        assert!(parse_list("server", "addr=localhost").is_err());
        assert!(parse_list("server", "id=oops").is_err());
        assert_eq!(parse_list("server", "").unwrap(), Vec::<ClientRow>::new());
    }

    #[test]
    fn colliding_ids_keep_server_scope_and_partial_failures_keep_healthy_rows() {
        let mut pane = ClientPane::default();
        pane.set_targets(BTreeMap::from([
            ("a".into(), "6379".into()),
            ("b".into(), "6380".into()),
        ]));
        for server in ["a", "b"] {
            pane.apply(
                server,
                Ok(ClientSnapshot {
                    rows: parse_list(server, "id=7 name=worker\nid=9 name=self").unwrap(),
                    own_id: 9,
                }),
                "",
            );
        }
        let scoped = |server: &str, id| ClientKey {
            server: server.into(),
            id,
        };
        assert_eq!(pane.rows.len(), 4);
        assert_eq!(pane.visible("'6380 worker").len(), 1);
        pane.select_all(true);
        assert_eq!(pane.marked, [scoped("a", 7), scoped("b", 7)].into());
        pane.ask_kill("'6379");
        assert_eq!(
            pane.kill_prompt(),
            "Kill 2 selected clients across 2 servers?"
        );
        let captured = BTreeMap::from([("a".into(), vec![7]), ("b".into(), vec![7])]);
        assert_eq!(pane.confirmation, Some(captured.clone()));
        pane.focus = Some(scoped("b", 7));
        pane.apply("a", Err("permission denied".into()), "");
        assert_eq!(pane.focus, Some(scoped("b", 7)));
        assert_eq!(pane.marked, [scoped("b", 7)].into());
        assert_eq!(pane.rows.len(), 2);
        assert_eq!(pane.confirmation, Some(captured));
        pane.toggle("'6380 self");
        assert!(pane.marked.contains(&scoped("b", 9)));
        pane.set_targets(BTreeMap::from([("a".into(), "6379".into())]));
        pane.apply(
            "b",
            Ok(ClientSnapshot {
                rows: parse_list("b", "id=7").unwrap(),
                own_id: 7,
            }),
            "",
        );
        assert_eq!(pane.rows, Vec::<ClientRow>::new());
    }

    #[test]
    fn selection_is_stable_and_bulk_skips_self_but_explicit_selection_allows_it() {
        let mut pane = ClientPane::default();
        pane.set_targets(BTreeMap::from([("server".into(), "server".into())]));
        pane.apply(
            "server",
            Ok(ClientSnapshot {
                rows: parse_list(
                    "server",
                    "id=1 name=alpha extra=needle\nid=2 name=beta\nid=3 name=self",
                )
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
        assert_eq!(pane.marked, [1, 2].map(key).into());
        pane.ask_kill("self");
        assert_eq!(pane.kill_prompt(), "Kill 2 selected clients?");
        pane.toggle("self");
        assert_eq!(pane.marked, [1, 2, 3].map(key).into());
        pane.rows.reverse();
        pane.move_focus("", 0);
        assert_eq!(pane.focus, Some(key(3)));
        pane.select_all(false);
        assert!(pane.marked.is_empty());
        pane.set_targets(BTreeMap::from([("other".into(), "other".into())]));
        assert_eq!(pane.rows, Vec::<ClientRow>::new());
        assert_eq!(pane.focus, None);
    }
}
