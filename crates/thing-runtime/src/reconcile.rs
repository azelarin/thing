use std::collections::{BTreeMap, BTreeSet};
use thing_core::{Node, NodeId};

pub(crate) fn depth(root: &Node, id: NodeId) -> Option<usize> {
    if root.id == id {
        Some(0)
    } else {
        root.children
            .iter()
            .find_map(|child| depth(child, id).map(|depth| depth + 1))
    }
}

pub(crate) fn ids(root: &Node, result: &mut BTreeSet<NodeId>) {
    result.insert(root.id);
    for child in &root.children {
        ids(child, result);
    }
}

/// keys match within a parent; unkeyed children match by position and kind.
pub(crate) fn preserve(old: &[Node], new: &mut [Node]) -> BTreeMap<NodeId, NodeId> {
    let keyed: BTreeMap<_, _> = old
        .iter()
        .filter_map(|node| node.key.as_deref().map(|key| (key, node)))
        .collect();
    let mut mapping = BTreeMap::new();
    for (index, node) in new.iter_mut().enumerate() {
        let previous = match node.key.as_deref() {
            Some(key) => keyed.get(key).copied(),
            None => old.get(index).filter(|node| node.key.is_none()),
        };
        if let Some(previous) = previous.filter(|previous| previous.kind == node.kind) {
            mapping.insert(node.id, previous.id);
            node.id = previous.id;
            mapping.extend(preserve(&previous.children, &mut node.children));
        }
    }
    mapping
}
