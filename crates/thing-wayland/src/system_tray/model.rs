use std::collections::BTreeMap;

use thing_runtime::{SystemTrayItem, SystemTraySnapshot};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Endpoint {
    pub service: String,
    pub path: String,
}

impl Endpoint {
    pub fn key(&self) -> String {
        format!("{}{}", self.service, self.path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Lifetime {
    pub id: String,
    pub serial: u64,
    pub endpoint: Endpoint,
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub id: String,
    pub endpoint: Endpoint,
    pub owner: String,
    pub interface: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueriedItem {
    pub item: SystemTrayItem,
    pub interface: String,
}

#[derive(Default)]
pub(super) struct Model {
    lifetimes: BTreeMap<Endpoint, Lifetime>,
    items: BTreeMap<Endpoint, (u64, SystemTrayItem, String)>,
}

pub(super) struct Projection {
    pub snapshot: SystemTraySnapshot,
    pub targets: BTreeMap<String, Target>,
}

impl Model {
    pub fn register(
        &mut self,
        endpoint: Endpoint,
        owner: String,
        next_id: &mut u64,
        limit: usize,
    ) -> Result<Option<Lifetime>, String> {
        if let Some(current) = self
            .lifetimes
            .get(&endpoint)
            .filter(|current| current.owner == owner)
        {
            return Ok(Some(current.clone()));
        }
        if !self.lifetimes.contains_key(&endpoint) && self.lifetimes.len() >= limit {
            return Ok(None);
        }
        self.items.remove(&endpoint);
        let serial = *next_id;
        *next_id = next_id
            .checked_add(1)
            .ok_or_else(|| "system tray item id space exhausted".to_owned())?;
        let lifetime = Lifetime {
            id: format!("tray{serial}"),
            serial,
            endpoint: endpoint.clone(),
            owner,
        };
        self.lifetimes.insert(endpoint, lifetime.clone());
        Ok(Some(lifetime))
    }

    pub fn update(&mut self, lifetime: &Lifetime, queried: QueriedItem) -> bool {
        let current = self
            .lifetimes
            .get(&lifetime.endpoint)
            .is_some_and(|current| current == lifetime);
        if !current || queried.item.id != lifetime.id {
            return false;
        }
        let value = (lifetime.serial, queried.item, queried.interface);
        if self.items.get(&lifetime.endpoint) == Some(&value) {
            return false;
        }
        self.items.insert(lifetime.endpoint.clone(), value);
        true
    }

    pub fn remove(&mut self, endpoint: &Endpoint, owner: Option<&str>) -> bool {
        let matches = self
            .lifetimes
            .get(endpoint)
            .is_some_and(|current| owner.is_none_or(|owner| current.owner == owner));
        if !matches {
            return false;
        }
        self.lifetimes.remove(endpoint);
        self.items.remove(endpoint);
        true
    }

    pub fn remove_name(&mut self, name: &str, owner: &str) -> bool {
        let endpoints = self
            .lifetimes
            .iter()
            .filter(|(_, lifetime)| {
                lifetime.owner == owner && (name == owner || lifetime.endpoint.service == name)
            })
            .map(|(endpoint, _)| endpoint.clone())
            .collect::<Vec<_>>();
        let changed = !endpoints.is_empty();
        for endpoint in endpoints {
            self.lifetimes.remove(&endpoint);
            self.items.remove(&endpoint);
        }
        changed
    }

    pub fn clear(&mut self) -> bool {
        let changed = !self.lifetimes.is_empty() || !self.items.is_empty();
        self.lifetimes.clear();
        self.items.clear();
        changed
    }

    pub fn contains_target(&self, target: &Target) -> bool {
        self.lifetimes
            .get(&target.endpoint)
            .is_some_and(|lifetime| {
                lifetime.id == target.id
                    && lifetime.owner == target.owner
                    && self
                        .items
                        .get(&target.endpoint)
                        .is_some_and(|(_, item, interface)| {
                            item.id == target.id && *interface == target.interface
                        })
            })
    }

    pub fn lifetimes_for_owner(&self, owner: &str) -> Vec<Lifetime> {
        self.lifetimes
            .values()
            .filter(|lifetime| lifetime.owner == owner)
            .cloned()
            .collect()
    }

    pub fn projection(&self, available: bool, watcher_owned: bool, error: String) -> Projection {
        let mut values = self.items.values().collect::<Vec<_>>();
        values.sort_by_key(|(serial, _, _)| *serial);
        let mut items = Vec::with_capacity(values.len());
        let mut targets = BTreeMap::new();
        for (_, item, interface) in values {
            let Some(lifetime) = self
                .lifetimes
                .values()
                .find(|lifetime| lifetime.id == item.id)
            else {
                continue;
            };
            targets.insert(
                item.id.clone(),
                Target {
                    id: item.id.clone(),
                    endpoint: lifetime.endpoint.clone(),
                    owner: lifetime.owner.clone(),
                    interface: interface.clone(),
                },
            );
            items.push(item.clone());
        }
        Projection {
            snapshot: SystemTraySnapshot {
                available,
                watcher_owned,
                error,
                items,
            },
            targets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_updates_and_removals_cannot_cross_owner_lifetimes() {
        let endpoint = Endpoint {
            service: "org.example.Item".into(),
            path: "/StatusNotifierItem".into(),
        };
        let mut model = Model::default();
        let mut next = 1;
        let first = model
            .register(endpoint.clone(), ":1.10".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        let second = model
            .register(endpoint.clone(), ":1.11".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(!model.update(&first, queried(&first)));
        assert!(!model.remove(&endpoint, Some(":1.10")));
        assert!(model.update(&second, queried(&second)));
        let projection = model.projection(true, false, String::new());
        assert_eq!(projection.snapshot.items[0].id, second.id);
        let target = projection.targets.get(&second.id).unwrap();
        assert!(model.contains_target(target));

        let third = model
            .register(endpoint, ":1.12".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        assert_ne!(third.id, second.id);
        assert!(!model.contains_target(target));
    }

    #[test]
    fn duplicate_registration_keeps_the_current_lifetime_id() {
        let endpoint = Endpoint {
            service: "org.example.Item".into(),
            path: "/StatusNotifierItem".into(),
        };
        let mut model = Model::default();
        let mut next = 1;
        let first = model
            .register(endpoint.clone(), ":1.10".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        let duplicate = model
            .register(endpoint, ":1.10".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        assert_eq!(first, duplicate);
        assert_eq!(next, 2);
    }

    #[test]
    fn releasing_one_well_known_name_keeps_other_items_from_the_same_owner() {
        let mut model = Model::default();
        let mut next = 1;
        let first = model
            .register(
                Endpoint {
                    service: "org.example.First".into(),
                    path: "/StatusNotifierItem".into(),
                },
                ":1.10".into(),
                &mut next,
                128,
            )
            .unwrap()
            .unwrap();
        let second = model
            .register(
                Endpoint {
                    service: "org.example.Second".into(),
                    path: "/StatusNotifierItem".into(),
                },
                ":1.10".into(),
                &mut next,
                128,
            )
            .unwrap()
            .unwrap();
        assert!(model.update(&first, queried(&first)));
        assert!(model.update(&second, queried(&second)));

        assert!(model.remove_name("org.example.First", ":1.10"));
        let projection = model.projection(true, false, String::new());
        assert_eq!(projection.snapshot.items.len(), 1);
        assert_eq!(projection.snapshot.items[0].id, second.id);

        assert!(model.remove_name(":1.10", ":1.10"));
        assert!(
            model
                .projection(true, false, String::new())
                .snapshot
                .items
                .is_empty()
        );
    }

    fn queried(lifetime: &Lifetime) -> QueriedItem {
        QueriedItem {
            item: SystemTrayItem {
                id: lifetime.id.clone(),
                protocol_item_id: "item".into(),
                category: "ApplicationStatus".into(),
                title: String::new(),
                status: "Active".into(),
                icon_name: String::new(),
                attention_icon_name: String::new(),
                overlay_icon_name: String::new(),
                tooltip_icon_name: String::new(),
                tooltip_title: String::new(),
                tooltip_text: String::new(),
                item_is_menu: false,
                has_menu: false,
                has_icon_pixmap: false,
                has_attention_icon_pixmap: false,
                has_overlay_icon_pixmap: false,
                has_tooltip_icon_pixmap: false,
            },
            interface: "org.kde.StatusNotifierItem".into(),
        }
    }
}
