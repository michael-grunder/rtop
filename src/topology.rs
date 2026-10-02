use std::collections::HashMap;

use crate::model::InstanceState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeGroup {
    pub group: String,
    pub roots: Vec<String>,
    pub children: HashMap<String, Vec<String>>,
}

pub fn build_tree_groups(instances: &HashMap<String, InstanceState>) -> Vec<TreeGroup> {
    if instances.is_empty() {
        return Vec::new();
    }

    let nodes: Vec<&InstanceState> = instances.values().collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots = Vec::new();
    let mut key_by_addr: HashMap<&str, &str> = HashMap::new();

    for node in &nodes {
        key_by_addr.insert(node.addr.as_str(), node.key.as_str());
    }

    for node in &nodes {
        match &node.parent_addr {
            Some(parent) => {
                let parent_key = if instances.contains_key(parent) {
                    Some(parent.as_str())
                } else {
                    key_by_addr.get(parent.as_str()).copied()
                };

                if let Some(parent_key) = parent_key {
                    if parent_key == node.key {
                        roots.push(node.key.clone());
                    } else {
                        children
                            .entry(parent_key.to_string())
                            .or_default()
                            .push(node.key.clone());
                    }
                } else {
                    // Keep unresolved children visible in tree mode.
                    roots.push(node.key.clone());
                }
            }
            None => roots.push(node.key.clone()),
        }
    }

    roots.sort();
    for value in children.values_mut() {
        value.sort();
    }

    vec![TreeGroup {
        group: "All".to_string(),
        roots,
        children,
    }]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::model::InstanceState;

    use super::build_tree_groups;

    #[test]
    fn builds_replication_tree() {
        let mut map = HashMap::new();

        let mut master = InstanceState::new("master".into(), "127.0.0.1:6379".into());
        master.cluster_id = Some("Standalone".into());
        let mut replica = InstanceState::new("replica".into(), "127.0.0.1:6380".into());
        replica.cluster_id = Some("Standalone".into());
        replica.parent_addr = Some("master".into());

        map.insert(master.key.clone(), master);
        map.insert(replica.key.clone(), replica);

        let groups = build_tree_groups(&map);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].roots, vec!["master"]);
        assert_eq!(
            groups[0].children.get("master"),
            Some(&vec!["replica".into()])
        );
    }

    #[test]
    fn resolves_parent_by_address_when_key_differs() {
        let mut map = HashMap::new();

        let mut primary = InstanceState::new("alpha-primary".into(), "10.0.0.1:6379".into());
        primary.cluster_id = Some("Standalone".into());
        let mut replica = InstanceState::new("alpha-replica".into(), "10.0.0.2:6379".into());
        replica.cluster_id = Some("Standalone".into());
        replica.parent_addr = Some("10.0.0.1:6379".into());

        map.insert(primary.key.clone(), primary);
        map.insert(replica.key.clone(), replica);

        let groups = build_tree_groups(&map);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].roots, vec!["alpha-primary"]);
        assert_eq!(
            groups[0].children.get("alpha-primary"),
            Some(&vec!["alpha-replica".into()])
        );
    }

    #[test]
    fn keeps_replica_visible_when_parent_unresolved() {
        let mut map = HashMap::new();

        let mut primary = InstanceState::new("master".into(), "127.0.0.1:6379".into());
        primary.cluster_id = Some("Standalone".into());
        let mut replica = InstanceState::new("replica".into(), "127.0.0.1:6380".into());
        replica.cluster_id = Some("Standalone".into());
        replica.parent_addr = Some("missing-master".into());

        map.insert(primary.key.clone(), primary);
        map.insert(replica.key.clone(), replica);

        let groups = build_tree_groups(&map);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].roots, vec!["master", "replica"]);
        assert!(groups[0].children.is_empty());
    }
}
