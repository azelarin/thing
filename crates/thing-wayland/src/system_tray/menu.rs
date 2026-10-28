use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    sync::Arc,
};

use thing_runtime::{SystemTrayMenu, SystemTrayMenuItem};

use super::model::{Endpoint, Lifetime};

pub(super) const MAX_MENU_CHILDREN: usize = 64;
pub(super) const MAX_MENU_DEPTH: usize = 16;
pub(super) const MAX_MENU_NODES: usize = 256;
pub(super) const MAX_TOTAL_MENU_NODES: usize = 2048;
pub(super) const MAX_MENU_RETAINED_BYTES: usize = 512 * 1024;
pub(super) const MAX_TOTAL_MENU_RETAINED_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_MENU_CONSUMERS: usize = 256;
pub(super) const MAX_MENU_CONSUMER_BYTES: usize = 128;
const CONTAINER_ENTRY_OVERHEAD: usize = 4 * size_of::<usize>();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MenuRegistration {
    pub item_id: String,
    pub item_serial: u64,
    pub endpoint: Endpoint,
    pub owner: String,
    pub path: String,
    pub connection_epoch: u64,
}

impl MenuRegistration {
    fn from_lifetime(lifetime: Lifetime, path: String, connection_epoch: u64) -> Self {
        Self {
            item_id: compact(&lifetime.id),
            item_serial: lifetime.serial,
            endpoint: Endpoint {
                service: compact(&lifetime.endpoint.service),
                path: compact(&lifetime.endpoint.path),
            },
            owner: compact(&lifetime.owner),
            path: compact(&path),
            connection_epoch,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MenuProperties {
    pub kind: String,
    pub raw_label: String,
    pub enabled: bool,
    pub visible: bool,
    pub icon_name: String,
    pub shortcut: Vec<Vec<String>>,
    pub toggle_type: String,
    pub toggle_state: i32,
    pub disposition: String,
    pub has_submenu: bool,
}

impl MenuProperties {
    pub fn protocol_defaults() -> Self {
        Self {
            kind: "standard".into(),
            enabled: true,
            visible: true,
            toggle_state: -1,
            disposition: "normal".into(),
            ..Self::default()
        }
    }

    pub fn compact(mut self) -> Self {
        self.kind = compact(&self.kind);
        self.raw_label = compact(&self.raw_label);
        self.icon_name = compact(&self.icon_name);
        self.toggle_type = compact(&self.toggle_type);
        self.disposition = compact(&self.disposition);
        self.shortcut = compact_shortcuts(self.shortcut);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LayoutNode {
    pub protocol_id: i32,
    pub properties: MenuProperties,
    pub children: Vec<LayoutNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MenuLayout {
    pub revision: u32,
    pub parent: LayoutNode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MenuPropertyDelta {
    pub kind: Option<String>,
    pub raw_label: Option<String>,
    pub enabled: Option<bool>,
    pub visible: Option<bool>,
    pub icon_name: Option<String>,
    pub shortcut: Option<Vec<Vec<String>>>,
    pub toggle_type: Option<String>,
    pub toggle_state: Option<i32>,
    pub disposition: Option<String>,
    pub has_submenu: Option<bool>,
    pub removed: BTreeSet<String>,
}

impl MenuPropertyDelta {
    pub fn compact(mut self) -> Self {
        self.kind = self.kind.map(|value| compact(&value));
        self.raw_label = self.raw_label.map(|value| compact(&value));
        self.icon_name = self.icon_name.map(|value| compact(&value));
        self.shortcut = self.shortcut.map(compact_shortcuts);
        self.toggle_type = self.toggle_type.map(|value| compact(&value));
        self.disposition = self.disposition.map(|value| compact(&value));
        self.removed = self
            .removed
            .into_iter()
            .map(|value| compact(&value))
            .collect();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MenuTarget {
    pub id: String,
    pub registration: Arc<MenuRegistration>,
    pub protocol_id: i32,
    pub parent_id: Option<i32>,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    pub has_submenu: bool,
    pub branch_incarnation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FetchToken {
    pub target: MenuTarget,
    pub invalidation: u64,
    pub operation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OpenDecision {
    pub first_lease: bool,
    pub needs_layout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CloseDecision {
    pub closed: Vec<MenuTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayoutCommit {
    Applied,
    Superseded,
}

#[derive(Clone, Debug)]
struct Branch {
    loaded: bool,
    loading: bool,
    dirty: bool,
    invalidation: u64,
    error: String,
    children: Vec<i32>,
    leases: BTreeSet<String>,
    about_pending: bool,
    active_fetch: Option<u64>,
    last_used: u64,
}

impl Default for Branch {
    fn default() -> Self {
        Self {
            loaded: false,
            loading: false,
            dirty: true,
            invalidation: 0,
            error: String::new(),
            children: Vec::new(),
            leases: BTreeSet::new(),
            about_pending: false,
            active_fetch: None,
            last_used: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct StoredNode {
    id: String,
    incarnation: u64,
    protocol_id: i32,
    parent_id: Option<i32>,
    properties: MenuProperties,
    branch: Branch,
}

#[derive(Clone, Debug)]
struct StoredMenu {
    registration: Arc<MenuRegistration>,
    revision: u32,
    nodes: BTreeMap<i32, StoredNode>,
}

impl StoredMenu {
    fn root(&self) -> &StoredNode {
        self.nodes.get(&0).expect("menu root disappeared")
    }

    fn retained_bytes(&self) -> Option<usize> {
        self.nodes.values().try_fold(
            size_of::<Self>()
                .checked_add(size_of::<MenuRegistration>())?
                .checked_add(self.registration.item_id.capacity())?
                .checked_add(self.registration.owner.capacity())?
                .checked_add(self.registration.endpoint.service.capacity())?
                .checked_add(self.registration.endpoint.path.capacity())?
                .checked_add(self.registration.path.capacity())?,
            |total, node| {
                total
                    .checked_add(size_of::<i32>())?
                    .checked_add(CONTAINER_ENTRY_OVERHEAD)?
                    .checked_add(node.retained_bytes()?)
            },
        )
    }
}

impl StoredNode {
    fn retained_bytes(&self) -> Option<usize> {
        let shortcut_bytes = self.properties.shortcut.iter().try_fold(
            self.properties
                .shortcut
                .capacity()
                .checked_mul(size_of::<Vec<String>>())?,
            |total, chord| {
                chord.iter().try_fold(
                    total.checked_add(chord.capacity().checked_mul(size_of::<String>())?)?,
                    |total, part| total.checked_add(part.capacity()),
                )
            },
        )?;
        self.branch.leases.iter().try_fold(
            size_of::<Self>()
                .checked_add(self.id.capacity())?
                .checked_add(self.properties.kind.capacity())?
                .checked_add(self.properties.raw_label.capacity())?
                .checked_add(self.properties.icon_name.capacity())?
                .checked_add(self.properties.toggle_type.capacity())?
                .checked_add(self.properties.disposition.capacity())?
                .checked_add(self.branch.error.capacity())?
                .checked_add(shortcut_bytes)?
                .checked_add(
                    self.branch
                        .children
                        .capacity()
                        .checked_mul(size_of::<i32>())?,
                )?,
            |total, lease| {
                total
                    .checked_add(size_of::<String>())?
                    .checked_add(CONTAINER_ENTRY_OVERHEAD)?
                    .checked_add(lease.capacity())
            },
        )
    }
}

#[derive(Default)]
pub(super) struct MenuModel {
    menus: BTreeMap<String, StoredMenu>,
    retired: Vec<MenuTarget>,
    next_fetch: u64,
    tick: u64,
}

impl MenuModel {
    pub fn reconcile(
        &mut self,
        registrations: Vec<(Lifetime, String)>,
        connection_epoch: u64,
        next_id: &mut u64,
    ) -> Result<bool, String> {
        let desired = registrations
            .into_iter()
            .map(|(lifetime, path)| {
                let registration = Arc::new(MenuRegistration::from_lifetime(
                    lifetime,
                    path,
                    connection_epoch,
                ));
                (registration.item_id.clone(), registration)
            })
            .collect::<BTreeMap<_, _>>();
        let changed = self.menus.len() != desired.len()
            || self
                .menus
                .iter()
                .any(|(item_id, menu)| desired.get(item_id) != Some(&menu.registration));
        if !changed {
            return Ok(false);
        }
        let mut staged = self.menus.clone();
        staged.retain(|item, menu| {
            desired
                .get(item)
                .is_some_and(|registration| registration == &menu.registration)
        });
        for (item_id, registration) in desired {
            if staged.contains_key(&item_id) {
                continue;
            }
            let id = allocate_id(next_id)?;
            let incarnation = id_serial(&id)?;
            staged.insert(
                item_id,
                StoredMenu {
                    registration,
                    revision: 0,
                    nodes: BTreeMap::from([(
                        0,
                        StoredNode {
                            id,
                            incarnation,
                            protocol_id: 0,
                            parent_id: None,
                            properties: MenuProperties::protocol_defaults(),
                            branch: Branch::default(),
                        },
                    )]),
                },
            );
        }
        for menu in staged.values_mut() {
            fit_menu(menu, None)?;
        }
        fit_collection(&mut staged, None)?;
        for menu in self.menus.values().filter(|menu| {
            staged
                .get(&menu.registration.item_id)
                .is_none_or(|current| current.registration != menu.registration)
        }) {
            self.retired.extend(open_targets(menu));
        }
        self.menus = staged;
        Ok(changed)
    }

    pub fn clear(&mut self) -> bool {
        let changed = !self.menus.is_empty();
        for menu in self.menus.values() {
            self.retired.extend(open_targets(menu));
        }
        self.menus.clear();
        changed
    }

    pub fn take_retired(&mut self) -> Vec<MenuTarget> {
        std::mem::take(&mut self.retired)
    }

    pub fn projection(&self, item_id: &str) -> Option<Arc<SystemTrayMenu>> {
        let menu = self.menus.get(item_id)?;
        let root = menu.root();
        Some(Arc::new(SystemTrayMenu {
            id: root.id.clone(),
            loaded: root.branch.loaded,
            loading: root.branch.loading,
            error: root.branch.error.clone(),
            children: project_children(menu, root, 0),
        }))
    }

    pub fn targets(&self) -> BTreeMap<String, MenuTarget> {
        self.menus
            .values()
            .flat_map(|menu| {
                menu.nodes.values().map(|node| {
                    let target = make_target(&menu.registration, node);
                    (target.id.clone(), target)
                })
            })
            .collect()
    }

    pub fn consumers(&self) -> BTreeSet<String> {
        self.menus
            .values()
            .flat_map(|menu| menu.nodes.values())
            .flat_map(|node| node.branch.leases.iter().cloned())
            .collect()
    }

    pub fn contains_target(&self, target: &MenuTarget) -> bool {
        self.current_node(target).is_some()
    }

    pub fn can_activate(&self, target: &MenuTarget) -> bool {
        self.current_node(target).is_some_and(|node| {
            node.parent_id.is_some()
                && node.properties.visible
                && node.properties.enabled
                && node.properties.kind != "separator"
        })
    }

    pub fn open(&mut self, target: &MenuTarget, consumer: &str) -> Result<OpenDecision, String> {
        if consumer.is_empty()
            || consumer.len() > MAX_MENU_CONSUMER_BYTES
            || consumer.contains('\0')
        {
            return Err("invalid system tray menu consumer".into());
        }
        let (item_id, protocol_id) = self
            .current_location(target)
            .ok_or_else(|| "system tray menu is no longer available".to_owned())?;
        let current = &self.menus[&item_id].nodes[&protocol_id];
        if current.parent_id.is_some()
            && (!current.properties.has_submenu
                || !current.properties.enabled
                || !current.properties.visible
                || current.properties.kind == "separator")
        {
            return Err("system tray menu branch cannot be opened".into());
        }
        let known_consumer = self.menus.values().any(|menu| {
            menu.nodes
                .values()
                .any(|node| node.branch.leases.contains(consumer))
        });
        if !known_consumer {
            let consumer_count = self
                .menus
                .values()
                .flat_map(|menu| menu.nodes.values())
                .flat_map(|node| node.branch.leases.iter())
                .collect::<BTreeSet<_>>()
                .len();
            if consumer_count >= MAX_MENU_CONSUMERS {
                return Err("too many system tray menu consumers".into());
            }
        }
        let mut candidate = self.menus[&item_id].clone();
        let node = candidate
            .nodes
            .get_mut(&protocol_id)
            .expect("current menu item disappeared");
        let first_lease = node.branch.leases.is_empty();
        let inserted = node
            .branch
            .leases
            .insert(consumer.to_owned().into_boxed_str().into_string());
        if !inserted {
            return Ok(OpenDecision {
                first_lease: false,
                needs_layout: false,
            });
        }
        if first_lease {
            node.branch.about_pending = true;
        }
        let next_tick = self.tick.saturating_add(1);
        node.branch.last_used = next_tick;
        let needs_layout = !node.branch.loaded || node.branch.dirty;
        fit_menu(&mut candidate, Some(protocol_id))?;
        self.commit_candidate(item_id, candidate, Some(protocol_id))?;
        self.tick = next_tick;
        Ok(OpenDecision {
            first_lease,
            needs_layout,
        })
    }

    pub fn close(
        &mut self,
        target: &MenuTarget,
        consumer: &str,
    ) -> Result<CloseDecision, &'static str> {
        let (item_id, protocol_id) = self
            .current_location(target)
            .ok_or("system tray menu is no longer available")?;
        let menu = self
            .menus
            .get_mut(&item_id)
            .expect("current menu disappeared");
        let registration = menu.registration.clone();
        let ids = subtree_ids(menu, protocol_id);
        let mut closed = Vec::new();
        for id in ids.into_iter().rev() {
            let node = menu.nodes.get_mut(&id).expect("menu subtree changed");
            let was_open = !node.branch.leases.is_empty();
            node.branch.leases.remove(consumer);
            if was_open && node.branch.leases.is_empty() {
                node.branch.about_pending = false;
                node.branch.invalidation = node.branch.invalidation.saturating_add(1);
                closed.push(make_target(&registration, node));
            }
        }
        Ok(CloseDecision { closed })
    }

    pub fn release_consumer(&mut self, consumer: &str) -> Vec<MenuTarget> {
        let mut closed = Vec::new();
        for menu in self.menus.values_mut() {
            let registration = menu.registration.clone();
            let ids = menu.nodes.keys().copied().collect::<Vec<_>>();
            for id in ids.into_iter().rev() {
                let node = menu.nodes.get_mut(&id).expect("menu item disappeared");
                let was_open = !node.branch.leases.is_empty();
                node.branch.leases.remove(consumer);
                if was_open && node.branch.leases.is_empty() {
                    node.branch.about_pending = false;
                    node.branch.invalidation = node.branch.invalidation.saturating_add(1);
                    closed.push(make_target(&registration, node));
                }
            }
        }
        closed
    }

    #[cfg(test)]
    pub fn fetch_token(&mut self, target: &MenuTarget) -> Option<FetchToken> {
        self.prepare_fetch(target, false).ok().flatten()
    }

    pub fn registrations_for_signal(&self, owner: &str, path: &str) -> Vec<Arc<MenuRegistration>> {
        self.menus
            .values()
            .filter(|menu| menu.registration.owner == owner && menu.registration.path == path)
            .map(|menu| menu.registration.clone())
            .collect()
    }

    pub fn invalidate_branch(
        &mut self,
        registration: &MenuRegistration,
        protocol_id: i32,
    ) -> Option<MenuTarget> {
        let menu = self.menus.get_mut(&registration.item_id)?;
        if menu.registration.as_ref() != registration {
            return None;
        }
        let registration = menu.registration.clone();
        let node = menu.nodes.get_mut(&protocol_id)?;
        node.branch.invalidation = node.branch.invalidation.saturating_add(1);
        node.branch.dirty = true;
        (!node.branch.leases.is_empty()).then(|| make_target(&registration, node))
    }

    pub fn can_query(&self, target: &MenuTarget) -> bool {
        self.current_node(target).is_some_and(|node| {
            !node.branch.leases.is_empty()
                && (node.parent_id.is_none()
                    || (node.properties.has_submenu
                        && node.properties.enabled
                        && node.properties.visible
                        && node.properties.kind != "separator"))
        })
    }

    pub fn prepare_fetch(
        &mut self,
        target: &MenuTarget,
        about_to_show: bool,
    ) -> Result<Option<FetchToken>, String> {
        let Some((item_id, protocol_id)) = self.current_location(target) else {
            return Ok(None);
        };
        if !self.can_query(target) {
            return Ok(None);
        }
        if self.menus[&item_id].nodes[&protocol_id]
            .branch
            .active_fetch
            .is_some()
        {
            return Ok(None);
        }
        let operation = self.next_fetch;
        self.next_fetch = self
            .next_fetch
            .checked_add(1)
            .ok_or_else(|| "system tray menu fetch generation space exhausted".to_owned())?;
        let menu = self
            .menus
            .get_mut(&item_id)
            .expect("current menu disappeared");
        let node = menu
            .nodes
            .get_mut(&protocol_id)
            .expect("current menu item disappeared");
        node.branch.loading = true;
        node.branch.active_fetch = Some(operation);
        if about_to_show {
            node.branch.about_pending = false;
        }
        Ok(Some(FetchToken {
            target: target.clone(),
            invalidation: node.branch.invalidation,
            operation,
        }))
    }

    pub fn pending_leased_targets(&self) -> Vec<MenuTarget> {
        self.menus
            .values()
            .flat_map(|menu| {
                menu.nodes
                    .values()
                    .filter(|node| {
                        (node.branch.dirty || node.branch.about_pending)
                            && !node.branch.loading
                            && !node.branch.leases.is_empty()
                    })
                    .map(|node| make_target(&menu.registration, node))
            })
            .collect()
    }

    pub fn pending_about_to_show(&self, target: &MenuTarget) -> bool {
        self.current_node(target)
            .is_some_and(|node| node.branch.about_pending)
    }

    pub fn needs_layout(&self, target: &MenuTarget) -> bool {
        self.current_node(target)
            .is_some_and(|node| !node.branch.loaded || node.branch.dirty)
    }

    #[cfg(test)]
    pub fn mark_dirty(&mut self, target: &MenuTarget) -> bool {
        let Some((item_id, protocol_id)) = self.current_location(target) else {
            return false;
        };
        let menu = self
            .menus
            .get_mut(&item_id)
            .expect("current menu disappeared");
        let node = menu
            .nodes
            .get_mut(&protocol_id)
            .expect("current menu item disappeared");
        node.branch.invalidation = node.branch.invalidation.saturating_add(1);
        node.branch.dirty = true;
        true
    }

    pub fn mark_whole_menu_dirty(&mut self, registration: &MenuRegistration) -> Vec<MenuTarget> {
        let Some(menu) = self.menus.get_mut(&registration.item_id) else {
            return Vec::new();
        };
        if menu.registration.as_ref() != registration {
            return Vec::new();
        }
        let registration = menu.registration.clone();
        let mut open = Vec::new();
        let ids = menu.nodes.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let node = menu.nodes.get_mut(&id).expect("menu item disappeared");
            node.branch.invalidation = node.branch.invalidation.saturating_add(1);
            node.branch.dirty = true;
            if !node.branch.leases.is_empty() {
                open.push(make_target(&registration, node));
            }
        }
        open
    }

    pub fn apply_property_updates(
        &mut self,
        registration: &MenuRegistration,
        updates: &BTreeMap<i32, MenuPropertyDelta>,
    ) -> Result<bool, String> {
        let Some(current) = self.menus.get(&registration.item_id) else {
            return Ok(false);
        };
        if current.registration.as_ref() != registration {
            return Ok(false);
        }
        let mut candidate = current.clone();
        let mut changed = false;
        let mut touched = BTreeSet::new();
        for (protocol_id, delta) in updates {
            let Some(node) = candidate.nodes.get_mut(protocol_id) else {
                continue;
            };
            touched.insert(*protocol_id);
            let previous = node.properties.clone();
            apply_property_delta(&mut node.properties, delta);
            validate_properties(&node.properties)?;
            changed |= node.properties != previous;
        }
        let invalidated = touched
            .iter()
            .flat_map(|id| {
                [Some(*id), candidate.nodes[id].parent_id]
                    .into_iter()
                    .flatten()
            })
            .collect::<BTreeSet<_>>();
        for id in invalidated {
            let node = candidate
                .nodes
                .get_mut(&id)
                .expect("property-update parent disappeared");
            node.branch.invalidation = node.branch.invalidation.saturating_add(1);
            if node.branch.active_fetch.is_some() {
                node.branch.dirty = true;
            }
        }
        let lost_submenus = updates
            .keys()
            .filter(|id| {
                **id != 0
                    && candidate.nodes.get(id).is_some_and(|node| {
                        !branch_is_openable(node)
                            && (node.branch.loaded
                                || node.branch.loading
                                || !node.branch.children.is_empty()
                                || !node.branch.leases.is_empty())
                    })
            })
            .copied()
            .collect::<Vec<_>>();
        for id in lost_submenus {
            retire_branch(&mut candidate, id);
            changed = true;
        }
        if touched.is_empty() {
            return Ok(false);
        }
        fit_menu(&mut candidate, None)?;
        self.commit_candidate(registration.item_id.clone(), candidate, None)?;
        Ok(changed)
    }

    pub fn finish_error(&mut self, token: &FetchToken, error: String) -> bool {
        let Some((item_id, protocol_id)) = self.current_fetch_location(token) else {
            return false;
        };
        let full = sanitize(error, 4096);
        for error in [full.as_str(), "system tray menu request failed", ""] {
            let mut candidate = self.menus[&item_id].clone();
            let node = candidate
                .nodes
                .get_mut(&protocol_id)
                .expect("current menu item disappeared");
            node.branch.loading = false;
            node.branch.active_fetch = None;
            node.branch.error = compact(error);
            node.branch.dirty = node.branch.invalidation != token.invalidation;
            if fit_menu(&mut candidate, Some(protocol_id)).is_ok()
                && self
                    .commit_candidate(item_id.clone(), candidate, Some(protocol_id))
                    .is_ok()
            {
                return true;
            }
        }
        false
    }

    pub fn finish_without_layout(&mut self, token: &FetchToken) -> bool {
        let Some((item_id, protocol_id)) = self.current_fetch_location(token) else {
            return false;
        };
        let node = self
            .menus
            .get_mut(&item_id)
            .expect("current menu disappeared")
            .nodes
            .get_mut(&protocol_id)
            .expect("current menu item disappeared");
        node.branch.loading = false;
        node.branch.active_fetch = None;
        if node.branch.invalidation == token.invalidation {
            node.branch.error.clear();
        }
        true
    }

    pub fn apply_layout(
        &mut self,
        token: &FetchToken,
        layout: MenuLayout,
        next_id: &mut u64,
    ) -> Result<LayoutCommit, String> {
        let Some((item_id, protocol_id)) = self.current_fetch_location(token) else {
            return Ok(LayoutCommit::Superseded);
        };
        let current_invalidation = self.menus[&item_id].nodes[&protocol_id].branch.invalidation;
        if current_invalidation != token.invalidation {
            let node = self
                .menus
                .get_mut(&item_id)
                .expect("current menu disappeared")
                .nodes
                .get_mut(&protocol_id)
                .expect("current menu item disappeared");
            node.branch.loading = false;
            node.branch.active_fetch = None;
            node.branch.dirty = true;
            return Ok(LayoutCommit::Superseded);
        }
        validate_layout(&layout, token.target.protocol_id)?;
        let depth = node_depth(&self.menus[&item_id], protocol_id)
            .ok_or_else(|| "system tray menu contains an invalid parent chain".to_owned())?;
        if depth >= MAX_MENU_DEPTH && !layout.parent.children.is_empty() {
            return Err("system tray menu exceeds the maximum depth".into());
        }
        if layout.parent.children.iter().any(|child| {
            child.protocol_id != protocol_id
                && is_ancestor(&self.menus[&item_id], child.protocol_id, protocol_id)
        }) {
            return Err("system tray menu contains an ancestor as a child".into());
        }

        let mut candidate = self.menus[&item_id].clone();
        replace_branch(&mut candidate, protocol_id, layout.parent, next_id)?;
        candidate.revision = layout.revision;
        fit_menu(&mut candidate, Some(protocol_id))?;
        self.commit_candidate(item_id, candidate, Some(protocol_id))?;
        Ok(LayoutCommit::Applied)
    }

    fn commit_candidate(
        &mut self,
        item_id: String,
        candidate: StoredMenu,
        protected: Option<i32>,
    ) -> Result<(), String> {
        let mut staged = self.menus.clone();
        staged.insert(item_id.clone(), candidate);
        fit_collection(
            &mut staged,
            protected.map(|branch| (item_id.as_str(), branch)),
        )?;
        if let (Some(previous), Some(current)) = (self.menus.get(&item_id), staged.get(&item_id)) {
            self.retired.extend(retired_targets(previous, current));
        }
        self.menus = staged;
        Ok(())
    }

    fn current_location(&self, target: &MenuTarget) -> Option<(String, i32)> {
        self.menus
            .get(&target.registration.item_id)
            .filter(|menu| menu.registration == target.registration)
            .and_then(|menu| {
                menu.nodes.get(&target.protocol_id).filter(|node| {
                    node.id == target.id
                        && node.incarnation == target.branch_incarnation
                        && node.parent_id == target.parent_id
                })
            })
            .map(|node| (target.registration.item_id.clone(), node.protocol_id))
    }

    fn current_node(&self, target: &MenuTarget) -> Option<&StoredNode> {
        let (item_id, protocol_id) = self.current_location(target)?;
        self.menus.get(&item_id)?.nodes.get(&protocol_id)
    }

    fn current_fetch_location(&self, token: &FetchToken) -> Option<(String, i32)> {
        let (item_id, protocol_id) = self.current_location(&token.target)?;
        (self.menus[&item_id].nodes[&protocol_id].branch.active_fetch == Some(token.operation))
            .then_some((item_id, protocol_id))
    }
}

fn fit_collection(
    menus: &mut BTreeMap<String, StoredMenu>,
    protected: Option<(&str, i32)>,
) -> Result<(), String> {
    loop {
        let total_nodes = menus
            .values()
            .try_fold(0_usize, |total, menu| total.checked_add(menu.nodes.len()))
            .ok_or_else(|| "system tray menu node-count overflow".to_owned())?;
        let total_bytes = menus
            .iter()
            .try_fold(0_usize, |total, (item_id, menu)| {
                total
                    .checked_add(size_of::<String>())?
                    .checked_add(CONTAINER_ENTRY_OVERHEAD)?
                    .checked_add(item_id.capacity())?
                    .checked_add(menu.retained_bytes()?)
            })
            .ok_or_else(|| "system tray menu retained-size overflow".to_owned())?;
        if total_nodes <= MAX_TOTAL_MENU_NODES && total_bytes <= MAX_TOTAL_MENU_RETAINED_BYTES {
            return Ok(());
        }
        let (protected_item, protected_branch) = protected.unwrap_or(("", i32::MIN));
        let Some((menu_id, branch_id)) =
            oldest_evictable_branch(menus, protected_item, protected.map(|_| protected_branch))
        else {
            return Err("system tray menus exceed the aggregate retained-data limit".into());
        };
        let menu = menus
            .get_mut(&menu_id)
            .expect("eviction candidate disappeared");
        evict_branch(menu, branch_id);
    }
}

fn open_targets(menu: &StoredMenu) -> Vec<MenuTarget> {
    menu.nodes
        .values()
        .filter(|node| !node.branch.leases.is_empty())
        .map(|node| make_target(&menu.registration, node))
        .collect()
}

fn replace_branch(
    menu: &mut StoredMenu,
    parent_id: i32,
    layout: LayoutNode,
    next_id: &mut u64,
) -> Result<(), String> {
    let mut parent_properties = layout.properties;
    if parent_id != 0 {
        // The branch was addressable as a submenu when it was opened. Some
        // implementations omit children-display from the returned parent.
        parent_properties.has_submenu = true;
    }
    if parent_id != 0 {
        let parent = menu
            .nodes
            .get_mut(&parent_id)
            .expect("validated menu parent disappeared");
        parent.properties = parent_properties;
        parent.branch.error.clear();
        if !branch_is_openable(parent) {
            retire_branch(menu, parent_id);
            return Ok(());
        }
    }
    let incoming = layout
        .children
        .iter()
        .map(|node| node.protocol_id)
        .collect::<BTreeSet<_>>();
    let old_children = menu.nodes[&parent_id].branch.children.clone();
    for child in old_children {
        if !incoming.contains(&child) {
            remove_subtree(menu, child);
        }
    }

    let mut children = Vec::with_capacity(layout.children.len());
    for child in layout.children {
        let protocol_id = child.protocol_id;
        let same_parent = menu
            .nodes
            .get(&protocol_id)
            .is_some_and(|node| node.parent_id == Some(parent_id));
        if !same_parent && menu.nodes.contains_key(&protocol_id) {
            remove_subtree(menu, protocol_id);
        }
        if let Some(node) = menu.nodes.get_mut(&protocol_id) {
            node.properties = child.properties;
        } else {
            let id = allocate_id(next_id)?;
            let incarnation = id_serial(&id)?;
            menu.nodes.insert(
                protocol_id,
                StoredNode {
                    id,
                    incarnation,
                    protocol_id,
                    parent_id: Some(parent_id),
                    properties: child.properties,
                    branch: Branch::default(),
                },
            );
        }
        if menu.nodes.get(&protocol_id).is_some_and(|node| {
            !branch_is_openable(node)
                && (node.branch.loaded
                    || node.branch.loading
                    || !node.branch.children.is_empty()
                    || !node.branch.leases.is_empty())
        }) {
            retire_branch(menu, protocol_id);
        }
        children.push(protocol_id);
    }
    let parent = menu
        .nodes
        .get_mut(&parent_id)
        .expect("validated menu parent disappeared");
    parent.branch.children = children;
    parent.branch.loaded = true;
    parent.branch.loading = false;
    parent.branch.active_fetch = None;
    parent.branch.dirty = false;
    parent.branch.error = String::new();
    Ok(())
}

fn branch_is_openable(node: &StoredNode) -> bool {
    node.properties.has_submenu
        && node.properties.enabled
        && node.properties.visible
        && node.properties.kind != "separator"
}

fn oldest_evictable_branch(
    menus: &BTreeMap<String, StoredMenu>,
    protected_item: &str,
    protected_branch: Option<i32>,
) -> Option<(String, i32)> {
    menus
        .iter()
        .flat_map(|(item_id, menu)| {
            menu.nodes.values().filter_map(move |node| {
                let protects_requested_branch = item_id == protected_item
                    && protected_branch.is_some_and(|protected| {
                        subtree_ids(menu, node.protocol_id).contains(&protected)
                    });
                (!protects_requested_branch
                    && !node.branch.children.is_empty()
                    && subtree_is_idle(menu, node.protocol_id))
                .then(|| (node.branch.last_used, item_id.clone(), node.protocol_id))
            })
        })
        .min_by_key(|(used, _, _)| *used)
        .map(|(_, item_id, protocol_id)| (item_id, protocol_id))
}

fn validate_layout(layout: &MenuLayout, expected_parent: i32) -> Result<(), String> {
    if layout.parent.protocol_id != expected_parent {
        return Err("system tray menu returned a different parent".into());
    }
    if layout.parent.children.len() > MAX_MENU_CHILDREN {
        return Err("system tray menu has too many direct children".into());
    }
    let mut ids = BTreeSet::new();
    for child in &layout.parent.children {
        if child.protocol_id == 0 || !child.children.is_empty() || !ids.insert(child.protocol_id) {
            return Err("system tray menu returned an invalid depth-one layout".into());
        }
        validate_properties(&child.properties)?;
    }
    if ids.contains(&expected_parent) {
        return Err("system tray menu contains its parent as a child".into());
    }
    validate_properties(&layout.parent.properties)
}

fn validate_properties(properties: &MenuProperties) -> Result<(), String> {
    let strings = [
        &properties.kind,
        &properties.raw_label,
        &properties.icon_name,
        &properties.toggle_type,
        &properties.disposition,
    ];
    if strings
        .into_iter()
        .any(|value| value.len() > 4096 || value.contains('\0'))
        || properties.shortcut.len() > 8
        || properties.shortcut.iter().any(|chord| {
            chord.len() > 8
                || chord
                    .iter()
                    .any(|part| part.len() > 64 || part.contains('\0'))
        })
    {
        return Err("system tray menu contains invalid properties".into());
    }
    Ok(())
}

fn apply_property_delta(properties: &mut MenuProperties, delta: &MenuPropertyDelta) {
    let defaults = MenuProperties::protocol_defaults();
    macro_rules! property {
        ($field:ident, $name:literal) => {
            if delta.removed.contains($name) {
                properties.$field = defaults.$field;
            } else if let Some(value) = &delta.$field {
                properties.$field = value.clone();
            }
        };
    }
    property!(kind, "type");
    property!(raw_label, "label");
    property!(enabled, "enabled");
    property!(visible, "visible");
    property!(icon_name, "icon-name");
    property!(shortcut, "shortcut");
    property!(toggle_type, "toggle-type");
    property!(toggle_state, "toggle-state");
    property!(disposition, "disposition");
    property!(has_submenu, "children-display");
}

fn make_target(registration: &Arc<MenuRegistration>, node: &StoredNode) -> MenuTarget {
    MenuTarget {
        id: node.id.clone(),
        registration: registration.clone(),
        protocol_id: node.protocol_id,
        parent_id: node.parent_id,
        enabled: node.properties.enabled,
        visible: node.properties.visible,
        separator: node.properties.kind == "separator",
        has_submenu: node.properties.has_submenu,
        branch_incarnation: node.incarnation,
    }
}

fn project_children(
    menu: &StoredMenu,
    parent: &StoredNode,
    depth: usize,
) -> Vec<SystemTrayMenuItem> {
    if depth >= MAX_MENU_DEPTH {
        return Vec::new();
    }
    parent
        .branch
        .children
        .iter()
        .filter_map(|id| menu.nodes.get(id))
        .map(|node| {
            let (label, access_key) = display_label(&node.properties.raw_label);
            SystemTrayMenuItem {
                id: node.id.clone(),
                kind: node.properties.kind.clone(),
                label,
                access_key,
                enabled: node.properties.enabled,
                visible: node.properties.visible,
                icon_name: node.properties.icon_name.clone(),
                shortcut: node.properties.shortcut.clone(),
                toggle_type: node.properties.toggle_type.clone(),
                toggle_state: node.properties.toggle_state,
                disposition: node.properties.disposition.clone(),
                has_submenu: node.properties.has_submenu,
                loaded: node.branch.loaded,
                loading: node.branch.loading,
                error: node.branch.error.clone(),
                children: project_children(menu, node, depth + 1),
            }
        })
        .collect()
}

fn display_label(raw: &str) -> (String, Option<String>) {
    let mut label = String::with_capacity(raw.len());
    let mut access_key = None;
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '_' {
            label.push(character);
            continue;
        }
        match characters.peek().copied() {
            Some('_') => {
                characters.next();
                label.push('_');
            }
            Some(next) => {
                access_key.get_or_insert_with(|| next.to_string());
            }
            None => {}
        }
    }
    (label, access_key)
}

fn subtree_ids(menu: &StoredMenu, root: i32) -> Vec<i32> {
    let mut result = vec![root];
    let mut index = 0;
    while index < result.len() {
        let id = result[index];
        if let Some(node) = menu.nodes.get(&id) {
            result.extend(node.branch.children.iter().copied());
        }
        index += 1;
    }
    result
}

fn node_depth(menu: &StoredMenu, mut id: i32) -> Option<usize> {
    let mut depth = 0_usize;
    let mut visited = BTreeSet::new();
    while let Some(parent) = menu.nodes.get(&id)?.parent_id {
        if !visited.insert(id) {
            return None;
        }
        depth = depth.checked_add(1)?;
        id = parent;
    }
    Some(depth)
}

fn is_ancestor(menu: &StoredMenu, ancestor: i32, mut descendant: i32) -> bool {
    let mut visited = BTreeSet::new();
    while let Some(parent) = menu.nodes.get(&descendant).and_then(|node| node.parent_id) {
        if parent == ancestor {
            return true;
        }
        if !visited.insert(descendant) {
            return false;
        }
        descendant = parent;
    }
    false
}

fn subtree_is_idle(menu: &StoredMenu, root: i32) -> bool {
    subtree_ids(menu, root).into_iter().all(|id| {
        menu.nodes
            .get(&id)
            .is_some_and(|node| node.branch.leases.is_empty() && !node.branch.loading)
    })
}

fn fit_menu(menu: &mut StoredMenu, protected: Option<i32>) -> Result<(), String> {
    loop {
        let bytes = menu
            .retained_bytes()
            .ok_or_else(|| "system tray menu retained-size overflow".to_owned())?;
        if menu.nodes.len() <= MAX_MENU_NODES && bytes <= MAX_MENU_RETAINED_BYTES {
            return Ok(());
        }
        let Some(branch) = oldest_evictable_in_menu(menu, protected) else {
            return Err("system tray menu exceeds its retained-data limit".into());
        };
        evict_branch(menu, branch);
    }
}

fn oldest_evictable_in_menu(menu: &StoredMenu, protected: Option<i32>) -> Option<i32> {
    menu.nodes
        .values()
        .filter(|node| {
            !node.branch.children.is_empty()
                && subtree_is_idle(menu, node.protocol_id)
                && protected.is_none_or(|protected| {
                    !subtree_ids(menu, node.protocol_id).contains(&protected)
                })
        })
        .min_by_key(|node| node.branch.last_used)
        .map(|node| node.protocol_id)
}

fn evict_branch(menu: &mut StoredMenu, parent: i32) {
    let children = menu
        .nodes
        .get(&parent)
        .map(|node| node.branch.children.clone())
        .unwrap_or_default();
    for child in children {
        remove_subtree(menu, child);
    }
    if let Some(parent) = menu.nodes.get_mut(&parent) {
        parent.branch.children.clear();
        parent.branch.loaded = false;
        parent.branch.loading = false;
        parent.branch.active_fetch = None;
        parent.branch.dirty = true;
        parent.branch.invalidation = parent.branch.invalidation.saturating_add(1);
    }
}

fn retire_branch(menu: &mut StoredMenu, parent: i32) {
    let ids = subtree_ids(menu, parent);
    for id in &ids {
        if let Some(node) = menu.nodes.get_mut(id) {
            node.branch.leases.clear();
            node.branch.about_pending = false;
            node.branch.loading = false;
            node.branch.active_fetch = None;
        }
    }
    evict_branch(menu, parent);
}

fn retired_targets(previous: &StoredMenu, current: &StoredMenu) -> Vec<MenuTarget> {
    previous
        .nodes
        .values()
        .filter(|node| {
            !node.branch.leases.is_empty()
                && current.nodes.get(&node.protocol_id).is_none_or(|current| {
                    current.incarnation != node.incarnation || current.branch.leases.is_empty()
                })
        })
        .map(|node| make_target(&previous.registration, node))
        .collect()
}

fn remove_subtree(menu: &mut StoredMenu, root: i32) {
    let ids = subtree_ids(menu, root);
    if let Some(parent_id) = menu.nodes.get(&root).and_then(|node| node.parent_id)
        && let Some(parent) = menu.nodes.get_mut(&parent_id)
    {
        parent.branch.children.retain(|child| *child != root);
    }
    for id in ids.into_iter().rev() {
        menu.nodes.remove(&id);
    }
}

fn allocate_id(next_id: &mut u64) -> Result<String, String> {
    let serial = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or_else(|| "system tray menu id space exhausted".to_owned())?;
    Ok(format!("tray-menu{serial}"))
}

fn id_serial(id: &str) -> Result<u64, String> {
    id.strip_prefix("tray-menu")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "invalid generated system tray menu id".to_owned())
}

fn sanitize(mut value: String, max: usize) -> String {
    value.retain(|character| character != '\0');
    if value.len() > max {
        let mut boundary = max;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    compact(&value)
}

fn compact(value: &str) -> String {
    value.to_owned().into_boxed_str().into_string()
}

fn compact_shortcuts(shortcuts: Vec<Vec<String>>) -> Vec<Vec<String>> {
    shortcuts
        .into_iter()
        .map(|chord| {
            chord
                .into_iter()
                .map(|part| compact(&part))
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .into_vec()
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
        .into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifetime(id: &str, serial: u64) -> Lifetime {
        Lifetime {
            id: id.into(),
            serial,
            endpoint: Endpoint {
                service: "org.example.Item".into(),
                path: "/StatusNotifierItem".into(),
            },
            owner: ":1.10".into(),
        }
    }

    fn child(id: i32, label: &str, submenu: bool) -> LayoutNode {
        LayoutNode {
            protocol_id: id,
            properties: MenuProperties {
                raw_label: label.into(),
                has_submenu: submenu,
                ..MenuProperties::protocol_defaults()
            },
            children: Vec::new(),
        }
    }

    fn layout(parent: i32, children: Vec<LayoutNode>) -> MenuLayout {
        MenuLayout {
            revision: 1,
            parent: LayoutNode {
                protocol_id: parent,
                properties: MenuProperties::protocol_defaults(),
                children,
            },
        }
    }

    fn dense_menu(index: usize) -> StoredMenu {
        let item_id = format!("tray{index}");
        let registration = Arc::new(MenuRegistration::from_lifetime(
            lifetime(&item_id, index as u64 + 1),
            "/Menu".into(),
            1,
        ));
        let mut root = StoredNode {
            id: format!("tray-menu-{index}-root"),
            incarnation: index as u64 + 1,
            protocol_id: 0,
            parent_id: None,
            properties: MenuProperties::protocol_defaults(),
            branch: Branch::default(),
        };
        root.branch.loaded = true;
        root.branch.dirty = false;
        root.branch.leases.insert("view".into());
        root.branch.children = (1..=MAX_MENU_CHILDREN as i32).collect();
        let mut nodes = BTreeMap::from([(0, root)]);
        for protocol_id in 1..=MAX_MENU_CHILDREN as i32 {
            nodes.insert(
                protocol_id,
                StoredNode {
                    id: format!("tray-menu-{index}-{protocol_id}"),
                    incarnation: (index * 100 + protocol_id as usize) as u64,
                    protocol_id,
                    parent_id: Some(0),
                    properties: MenuProperties::protocol_defaults(),
                    branch: Branch::default(),
                },
            );
        }
        StoredMenu {
            registration,
            revision: 1,
            nodes,
        }
    }

    fn root_target(model: &MenuModel) -> MenuTarget {
        model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 0)
            .unwrap()
    }

    #[test]
    fn registration_and_path_changes_rotate_every_identity() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let first = root_target(&model);
        let decision = model.open(&first, "output1-generation1").unwrap();
        assert!(decision.first_lease);
        let token = model.fetch_token(&first).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "_Open", false)]), &mut next)
            .unwrap();
        let child_id = model.projection("tray1").unwrap().children[0].id.clone();

        model
            .reconcile(vec![(lifetime("tray1", 1), "/Other".into())], 1, &mut next)
            .unwrap();
        assert_eq!(model.take_retired(), vec![first.clone()]);
        let second = root_target(&model);
        assert_ne!(first.id, second.id);
        assert!(!model.targets().contains_key(&child_id));

        model
            .reconcile(vec![(lifetime("tray2", 2), "/Other".into())], 2, &mut next)
            .unwrap();
        assert_ne!(second.id, root_target(&model).id);
    }

    #[test]
    fn property_changes_preserve_ids_but_removal_and_movement_do_not() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(
                &token,
                layout(0, vec![child(1, "_Open", true), child(2, "Quit", false)]),
                &mut next,
            )
            .unwrap();
        let first = model.projection("tray1").unwrap().children[0].clone();
        assert_eq!(first.label, "Open");
        assert_eq!(first.access_key.as_deref(), Some("O"));

        let refreshed_root = root_target(&model);
        let token = model.fetch_token(&refreshed_root).unwrap();
        model
            .apply_layout(
                &token,
                layout(0, vec![child(1, "Open recent", true)]),
                &mut next,
            )
            .unwrap();
        let changed = model.projection("tray1").unwrap().children[0].clone();
        assert_eq!(changed.id, first.id);

        let submenu = model.targets().get(&changed.id).unwrap().clone();
        model.open(&submenu, "view").unwrap();
        let token = model.fetch_token(&submenu).unwrap();
        model
            .apply_layout(&token, layout(1, vec![child(2, "Moved", false)]), &mut next)
            .unwrap();
        let moved = model.projection("tray1").unwrap().children[0].children[0].clone();
        assert_ne!(moved.id, first.id);
    }

    #[test]
    fn stale_fetch_cannot_clear_a_later_invalidation() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        assert!(model.mark_dirty(&root));
        assert_eq!(
            model
                .apply_layout(&token, layout(0, vec![child(1, "stale", false)]), &mut next)
                .unwrap(),
            LayoutCommit::Superseded
        );
        let menu = model.projection("tray1").unwrap();
        assert!(!menu.loaded);
        assert!(menu.children.is_empty());
    }

    #[test]
    fn root_property_updates_preserve_loaded_children_and_leases() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Open", false)]), &mut next)
            .unwrap();
        let registration = model.menus["tray1"].registration.clone();

        assert!(
            model
                .apply_property_updates(
                    &registration,
                    &BTreeMap::from([(
                        0,
                        MenuPropertyDelta {
                            raw_label: Some("Root".into()),
                            ..MenuPropertyDelta::default()
                        },
                    )]),
                )
                .unwrap()
        );

        let menu = model.projection("tray1").unwrap();
        assert!(menu.loaded);
        assert_eq!(menu.children.len(), 1);
        assert!(model.take_retired().is_empty());
        assert_eq!(model.close(&root, "view").unwrap().closed, vec![root]);
    }

    #[test]
    fn property_signal_during_parent_fetch_keeps_newer_properties_and_refetches() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let initial = model.fetch_token(&root).unwrap();
        model
            .apply_layout(
                &initial,
                layout(0, vec![child(1, "Before", false)]),
                &mut next,
            )
            .unwrap();

        let in_flight = model.fetch_token(&root).unwrap();
        let next_fetch = model.next_fetch;
        assert!(model.fetch_token(&root).is_none());
        assert_eq!(model.next_fetch, next_fetch);
        let registration = model.menus["tray1"].registration.clone();
        model
            .apply_property_updates(
                &registration,
                &BTreeMap::from([(
                    1,
                    MenuPropertyDelta {
                        raw_label: Some("After".into()),
                        ..MenuPropertyDelta::default()
                    },
                )]),
            )
            .unwrap();
        assert_eq!(
            model.projection("tray1").unwrap().children[0].label,
            "After"
        );

        assert_eq!(
            model
                .apply_layout(
                    &in_flight,
                    layout(0, vec![child(1, "Before", false)]),
                    &mut next,
                )
                .unwrap(),
            LayoutCommit::Superseded
        );
        assert_eq!(
            model.projection("tray1").unwrap().children[0].label,
            "After"
        );
        assert_eq!(model.pending_leased_targets(), vec![root.clone()]);
        assert!(model.fetch_token(&root).is_some());
    }

    #[test]
    fn closing_during_a_fetch_cannot_restore_the_lease() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        assert_eq!(
            model.close(&root, "view").unwrap().closed,
            vec![root.clone()]
        );

        assert_eq!(
            model
                .apply_layout(
                    &token,
                    layout(0, vec![child(1, "Cached", false)]),
                    &mut next
                )
                .unwrap(),
            LayoutCommit::Superseded
        );
        assert!(model.consumers().is_empty());
        assert!(!model.can_query(&root));
        assert!(model.pending_leased_targets().is_empty());
    }

    #[test]
    fn leases_are_per_consumer_and_release_descendants() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        assert!(model.open(&root, "output-a").unwrap().first_lease);
        assert!(!model.open(&root, "output-b").unwrap().first_lease);
        assert!(!model.open(&root, "output-a").unwrap().needs_layout);
        assert!(model.close(&root, "output-a").unwrap().closed.is_empty());
        let closed = model.release_consumer("output-b");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, root.id);
    }

    #[test]
    fn invalid_layouts_do_not_replace_last_good_state() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "kept", false)]), &mut next)
            .unwrap();
        let before = model.projection("tray1").unwrap();
        let root = root_target(&model);
        let token = model.fetch_token(&root).unwrap();
        let invalid = layout(0, vec![child(0, "bad", false)]);
        assert!(model.apply_layout(&token, invalid, &mut next).is_err());
        assert!(model.finish_error(&token, "invalid layout".into()));
        let after = model.projection("tray1").unwrap();
        assert!(after.loaded);
        assert!(!after.loading);
        assert_eq!(after.children, before.children);

        let token = model.fetch_token(&root).unwrap();
        let mut indeterminate_toggle = child(1, "toggle", false);
        indeterminate_toggle.properties.toggle_state = 2;
        model
            .apply_layout(&token, layout(0, vec![indeterminate_toggle]), &mut next)
            .unwrap();
        assert_eq!(
            model.projection("tray1").unwrap().children[0].toggle_state,
            2
        );
    }

    #[test]
    fn mnemonic_escaping_is_display_ready() {
        assert_eq!(
            display_label("_Open __recent_"),
            ("Open _recent".into(), Some("O".into()))
        );
    }

    #[test]
    fn leased_descendant_protects_its_ancestors_from_eviction() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "root-view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Recent", true)]), &mut next)
            .unwrap();
        model.close(&root, "root-view").unwrap();
        let submenu = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 1)
            .unwrap();
        model.open(&submenu, "submenu-view").unwrap();

        let menu = model.menus.get("tray1").unwrap();
        assert_eq!(oldest_evictable_in_menu(menu, None), None);
    }

    #[test]
    fn removing_submenu_state_retires_open_lifetimes() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Recent", true)]), &mut next)
            .unwrap();
        let submenu = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 1)
            .unwrap();
        model.open(&submenu, "view").unwrap();

        let registration = model.menus["tray1"].registration.clone();
        let mut removed = MenuPropertyDelta::default();
        removed.removed.insert("children-display".into());
        model
            .apply_property_updates(&registration, &BTreeMap::from([(1, removed)]))
            .unwrap();

        assert_eq!(model.take_retired(), vec![submenu]);
        let item = &model.projection("tray1").unwrap().children[0];
        assert!(!item.has_submenu);
        assert!(!item.loaded);
        assert!(item.children.is_empty());
    }

    #[test]
    fn parent_refresh_retires_a_child_that_stops_being_a_submenu() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "root-view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Recent", true)]), &mut next)
            .unwrap();
        let submenu = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 1)
            .unwrap();
        model.open(&submenu, "submenu-view").unwrap();
        let token = model.fetch_token(&submenu).unwrap();
        model
            .apply_layout(&token, layout(1, vec![child(2, "File", false)]), &mut next)
            .unwrap();

        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(
                &token,
                layout(0, vec![child(1, "Recent", false)]),
                &mut next,
            )
            .unwrap();

        assert_eq!(model.take_retired(), vec![submenu.clone()]);
        let item = &model.projection("tray1").unwrap().children[0];
        assert!(!item.has_submenu);
        assert!(!item.loaded);
        assert!(item.children.is_empty());
        assert!(!model.can_query(&submenu));
        assert!(model.open(&submenu, "other-view").is_err());
    }

    #[test]
    fn submenu_refresh_retires_a_parent_that_becomes_noninteractive() {
        let mut disabled = MenuProperties::protocol_defaults();
        disabled.enabled = false;
        let mut hidden = MenuProperties::protocol_defaults();
        hidden.visible = false;
        let mut separator = MenuProperties::protocol_defaults();
        separator.kind = "separator".into();

        for (index, parent_properties) in [disabled, hidden, separator].into_iter().enumerate() {
            let mut model = MenuModel::default();
            let mut next = 1;
            model
                .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
                .unwrap();
            let root = root_target(&model);
            model.open(&root, "root-view").unwrap();
            let token = model.fetch_token(&root).unwrap();
            model
                .apply_layout(&token, layout(0, vec![child(1, "Recent", true)]), &mut next)
                .unwrap();
            let submenu = model
                .targets()
                .into_values()
                .find(|target| target.protocol_id == 1)
                .unwrap();
            model.open(&submenu, "submenu-view").unwrap();
            if index == 0 {
                let failed = model.fetch_token(&submenu).unwrap();
                assert!(model.finish_error(&failed, "previous failure".into()));
                assert_eq!(
                    model.projection("tray1").unwrap().children[0].error,
                    "previous failure"
                );
            }
            let token = model.fetch_token(&submenu).unwrap();
            let mut response = layout(1, vec![child(2, "File", false)]);
            response.parent.properties = parent_properties;
            model.apply_layout(&token, response, &mut next).unwrap();

            assert_eq!(model.take_retired(), vec![submenu]);
            let item = &model.projection("tray1").unwrap().children[0];
            assert!(!item.enabled || !item.visible || item.kind == "separator");
            assert!(!item.loaded);
            assert!(item.children.is_empty());
            assert!(item.error.is_empty());
            assert_eq!(model.consumers(), BTreeSet::from(["root-view".into()]));
        }
    }

    #[test]
    fn layouts_cannot_insert_ancestors_or_exceed_the_depth_limit() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let mut parent = root_target(&model);
        for id in 1..=MAX_MENU_DEPTH as i32 {
            model.open(&parent, "view").unwrap();
            let token = model.fetch_token(&parent).unwrap();
            model
                .apply_layout(
                    &token,
                    layout(parent.protocol_id, vec![child(id, "Nested", true)]),
                    &mut next,
                )
                .unwrap();
            parent = model
                .targets()
                .into_values()
                .find(|target| target.protocol_id == id)
                .unwrap();
        }
        model.open(&parent, "view").unwrap();
        let token = model.fetch_token(&parent).unwrap();
        assert!(
            model
                .apply_layout(
                    &token,
                    layout(parent.protocol_id, vec![child(99, "Too deep", false)]),
                    &mut next,
                )
                .is_err()
        );

        let ancestor = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 2)
            .unwrap();
        let token = model.fetch_token(&ancestor).unwrap();
        assert!(
            model
                .apply_layout(
                    &token,
                    layout(2, vec![child(1, "Ancestor", true)]),
                    &mut next
                )
                .is_err()
        );
    }

    #[test]
    fn activation_rechecks_current_properties() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Open", false)]), &mut next)
            .unwrap();
        let target = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 1)
            .unwrap();
        assert!(model.can_activate(&target));

        let registration = model.menus["tray1"].registration.clone();
        let disabled = MenuPropertyDelta {
            enabled: Some(false),
            ..MenuPropertyDelta::default()
        };
        model
            .apply_property_updates(&registration, &BTreeMap::from([(1, disabled)]))
            .unwrap();
        assert!(!model.can_activate(&target));
    }

    #[test]
    fn opening_rechecks_current_visibility_enabled_state_and_kind() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        model.open(&root, "root-view").unwrap();
        let token = model.fetch_token(&root).unwrap();
        model
            .apply_layout(&token, layout(0, vec![child(1, "Recent", true)]), &mut next)
            .unwrap();
        let submenu = model
            .targets()
            .into_values()
            .find(|target| target.protocol_id == 1)
            .unwrap();
        let registration = model.menus["tray1"].registration.clone();

        for delta in [
            MenuPropertyDelta {
                enabled: Some(false),
                ..MenuPropertyDelta::default()
            },
            MenuPropertyDelta {
                enabled: Some(true),
                kind: Some("separator".into()),
                ..MenuPropertyDelta::default()
            },
            MenuPropertyDelta {
                kind: Some("standard".into()),
                visible: Some(false),
                ..MenuPropertyDelta::default()
            },
        ] {
            model
                .apply_property_updates(&registration, &BTreeMap::from([(1, delta)]))
                .unwrap();
            assert!(model.open(&submenu, "submenu-view").is_err());
        }
    }

    #[test]
    fn rejected_aggregate_update_does_not_evict_existing_menus() {
        let mut model = MenuModel::default();
        let menu_count = MAX_TOTAL_MENU_NODES / (MAX_MENU_CHILDREN + 1);
        for index in 0..menu_count {
            let menu = dense_menu(index);
            model.menus.insert(menu.registration.item_id.clone(), menu);
        }
        let before = model.menus.keys().cloned().collect::<Vec<_>>();
        let candidate = dense_menu(menu_count);
        let item_id = candidate.registration.item_id.clone();
        assert!(model.commit_candidate(item_id, candidate, Some(0)).is_err());
        assert_eq!(model.menus.keys().cloned().collect::<Vec<_>>(), before);
        assert_eq!(model.menus.len(), menu_count);
    }

    #[test]
    fn retained_strings_are_compacted_and_oversized_registrations_are_transactional() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        let mut consumer = String::with_capacity(MAX_MENU_RETAINED_BYTES);
        consumer.push_str("view");
        model.open(&root, &consumer).unwrap();
        assert_eq!(
            model.menus["tray1"]
                .root()
                .branch
                .leases
                .first()
                .unwrap()
                .capacity(),
            "view".len()
        );

        let token = model.fetch_token(&root).unwrap();
        let mut error = String::with_capacity(MAX_MENU_RETAINED_BYTES);
        error.extend(std::iter::repeat_n('x', 8192));
        assert!(model.finish_error(&token, error));
        assert_eq!(model.menus["tray1"].root().branch.error.capacity(), 4096);

        let first = root_target(&model);
        let oversized_path = format!("/{}", "x".repeat(MAX_MENU_RETAINED_BYTES));
        assert!(
            model
                .reconcile(vec![(lifetime("tray1", 1), oversized_path)], 1, &mut next,)
                .is_err()
        );
        assert_eq!(root_target(&model), first);
    }

    #[test]
    fn menu_consumers_have_a_process_wide_bound() {
        let mut model = MenuModel::default();
        let mut next = 1;
        model
            .reconcile(vec![(lifetime("tray1", 1), "/Menu".into())], 1, &mut next)
            .unwrap();
        let root = root_target(&model);
        for index in 0..MAX_MENU_CONSUMERS {
            model.open(&root, &format!("view-{index}")).unwrap();
        }
        assert_eq!(model.consumers().len(), MAX_MENU_CONSUMERS);
        assert!(model.open(&root, "one-too-many").is_err());
    }
}
