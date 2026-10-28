use std::collections::BTreeMap;

use thing_core::{ImageCandidate, ImagePixmap, ImageSource};
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
    pub icon: QueriedIcon,
    pub attention_icon: QueriedIcon,
    pub interface: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct QueriedIcon {
    pub name: String,
    pub pixmaps: Vec<QueriedPixmap>,
}

impl QueriedIcon {
    pub fn retained_bytes(&self) -> usize {
        self.pixmaps.iter().map(|pixmap| pixmap.rgba.len()).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueriedPixmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
struct StoredIcon {
    name: String,
    pixmaps: Vec<ImagePixmap>,
}

impl StoredIcon {
    fn from_query(query: QueriedIcon, previous: Option<&Self>) -> Result<Self, String> {
        if let Some(previous) = previous.filter(|previous| previous.matches(&query)) {
            return Ok(previous.clone());
        }
        let pixmaps = query
            .pixmaps
            .into_iter()
            .map(|pixmap| ImagePixmap::new(pixmap.width, pixmap.height, pixmap.rgba))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: query.name,
            pixmaps,
        })
    }

    fn matches(&self, query: &QueriedIcon) -> bool {
        self.name == query.name
            && self.pixmaps.len() == query.pixmaps.len()
            && self
                .pixmaps
                .iter()
                .zip(&query.pixmaps)
                .all(|(stored, queried)| {
                    stored.width() == queried.width
                        && stored.height() == queried.height
                        && stored.rgba() == queried.rgba
                })
    }

    fn candidate(&self) -> Result<Option<ImageCandidate>, String> {
        if self.name.is_empty() && self.pixmaps.is_empty() {
            return Ok(None);
        }
        ImageCandidate::new(
            (!self.name.is_empty()).then(|| self.name.clone()),
            self.pixmaps.clone(),
        )
        .map(Some)
    }

    fn retained_bytes(&self) -> usize {
        self.pixmaps.iter().map(|pixmap| pixmap.rgba().len()).sum()
    }
}

#[derive(Clone)]
struct StoredItem {
    serial: u64,
    item: SystemTrayItem,
    interface: String,
    icon: StoredIcon,
    attention_icon: StoredIcon,
}

impl StoredItem {
    fn retained_bytes(&self) -> usize {
        self.icon
            .retained_bytes()
            .saturating_add(self.attention_icon.retained_bytes())
    }
}

#[derive(Default)]
pub(super) struct Model {
    lifetimes: BTreeMap<Endpoint, Lifetime>,
    items: BTreeMap<Endpoint, StoredItem>,
    retained_image_bytes: usize,
    next_image_id: u64,
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
        if let Some(item) = self.items.remove(&endpoint) {
            self.retained_image_bytes -= item.retained_bytes();
        }
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

    pub fn update(
        &mut self,
        lifetime: &Lifetime,
        mut queried: QueriedItem,
        retained_limit: usize,
    ) -> Result<bool, String> {
        let current = self
            .lifetimes
            .get(&lifetime.endpoint)
            .is_some_and(|current| current == lifetime);
        if !current || queried.item.id != lifetime.id {
            return Ok(false);
        }
        let previous = self.items.get(&lifetime.endpoint).cloned();
        let previous_bytes = previous.as_ref().map_or(0, StoredItem::retained_bytes);
        let queried_bytes = queried
            .icon
            .retained_bytes()
            .checked_add(queried.attention_icon.retained_bytes())
            .ok_or_else(|| "system tray image budget overflow".to_owned())?;
        let projected = self
            .retained_image_bytes
            .checked_sub(previous_bytes)
            .and_then(|bytes| bytes.checked_add(queried_bytes))
            .ok_or_else(|| "system tray image budget overflow".to_owned())?;
        if projected > retained_limit {
            queried.icon.pixmaps.clear();
            queried.attention_icon.pixmaps.clear();
        }
        let icon = StoredIcon::from_query(
            queried.icon,
            previous.as_ref().map(|previous| &previous.icon),
        )?;
        let attention_icon = StoredIcon::from_query(
            queried.attention_icon,
            previous.as_ref().map(|previous| &previous.attention_icon),
        )?;
        queried.item.has_icon_pixmap = !icon.pixmaps.is_empty();
        queried.item.has_attention_icon_pixmap = !attention_icon.pixmaps.is_empty();
        let mut candidates = Vec::new();
        if queried.item.status == "NeedsAttention"
            && let Some(candidate) = attention_icon.candidate()?
        {
            candidates.push(candidate);
        }
        if let Some(candidate) = icon.candidate()? {
            candidates.push(candidate);
        }
        queried.item.icon = if candidates.is_empty() {
            None
        } else if previous
            .as_ref()
            .and_then(|previous| previous.item.icon.as_ref())
            .is_some_and(|source| source.candidates() == candidates)
        {
            previous
                .as_ref()
                .and_then(|previous| previous.item.icon.clone())
        } else {
            let image_id = self.next_image_id;
            self.next_image_id = self
                .next_image_id
                .checked_add(1)
                .ok_or_else(|| "system tray image id space exhausted".to_owned())?;
            Some(ImageSource::new(
                format!("{}-image{image_id}", lifetime.id),
                candidates,
            )?)
        };
        let value = StoredItem {
            serial: lifetime.serial,
            item: queried.item,
            interface: queried.interface,
            icon,
            attention_icon,
        };
        let changed = previous.as_ref().is_none_or(|previous| {
            previous.serial != value.serial
                || previous.item != value.item
                || previous.interface != value.interface
        });
        self.retained_image_bytes = self
            .retained_image_bytes
            .checked_sub(previous_bytes)
            .and_then(|bytes| bytes.checked_add(value.retained_bytes()))
            .ok_or_else(|| "system tray image budget overflow".to_owned())?;
        debug_assert!(self.retained_image_bytes <= retained_limit);
        self.items.insert(lifetime.endpoint.clone(), value);
        Ok(changed)
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
        if let Some(item) = self.items.remove(endpoint) {
            self.retained_image_bytes -= item.retained_bytes();
        }
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
            if let Some(item) = self.items.remove(&endpoint) {
                self.retained_image_bytes -= item.retained_bytes();
            }
        }
        changed
    }

    pub fn clear(&mut self) -> bool {
        let changed = !self.lifetimes.is_empty() || !self.items.is_empty();
        self.lifetimes.clear();
        self.items.clear();
        self.retained_image_bytes = 0;
        changed
    }

    pub fn contains_target(&self, target: &Target) -> bool {
        self.lifetimes
            .get(&target.endpoint)
            .is_some_and(|lifetime| {
                lifetime.id == target.id
                    && lifetime.owner == target.owner
                    && self.items.get(&target.endpoint).is_some_and(|item| {
                        item.item.id == target.id && item.interface == target.interface
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
        values.sort_by_key(|item| item.serial);
        let mut items = Vec::with_capacity(values.len());
        let mut targets = BTreeMap::new();
        for stored in values {
            let item = &stored.item;
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
                    interface: stored.interface.clone(),
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
        assert!(!model.update(&first, queried(&first), usize::MAX).unwrap());
        assert!(!model.remove(&endpoint, Some(":1.10")));
        assert!(model.update(&second, queried(&second), usize::MAX).unwrap());
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
        assert!(model.update(&first, queried(&first), usize::MAX).unwrap());
        assert!(model.update(&second, queried(&second), usize::MAX).unwrap());

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

    #[test]
    fn image_identity_tracks_content_and_owner_lifetimes() {
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
        assert!(
            model
                .update(&first, queried_with_icon(&first, 10), 4)
                .unwrap()
        );
        let first_source = model.projection(true, false, String::new()).snapshot.items[0]
            .icon
            .clone()
            .unwrap();
        assert!(
            !model
                .update(&first, queried_with_icon(&first, 10), 4)
                .unwrap()
        );
        assert_eq!(
            model.projection(true, false, String::new()).snapshot.items[0]
                .icon
                .as_ref()
                .unwrap()
                .id(),
            first_source.id()
        );

        assert!(
            model
                .update(&first, queried_with_icon(&first, 20), 4)
                .unwrap()
        );
        let changed_source = model.projection(true, false, String::new()).snapshot.items[0]
            .icon
            .clone()
            .unwrap();
        assert_ne!(changed_source.id(), first_source.id());

        let second = model
            .register(endpoint, ":1.11".into(), &mut next, 128)
            .unwrap()
            .unwrap();
        assert!(
            model
                .update(&second, queried_with_icon(&second, 20), 4)
                .unwrap()
        );
        let replacement = model.projection(true, false, String::new()).snapshot.items[0]
            .icon
            .clone()
            .unwrap();
        assert_ne!(replacement.id(), changed_source.id());
        assert!(replacement.id().starts_with(&second.id));
    }

    #[test]
    fn aggregate_image_budget_discards_pixels_without_dropping_metadata() {
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
                ":1.11".into(),
                &mut next,
                128,
            )
            .unwrap()
            .unwrap();
        assert!(
            model
                .update(&first, queried_with_icon(&first, 10), 4)
                .unwrap()
        );
        assert!(
            model
                .update(&second, queried_with_icon(&second, 20), 4)
                .unwrap()
        );

        let projection = model.projection(true, false, String::new());
        assert_eq!(projection.snapshot.items.len(), 2);
        let over_budget = projection
            .snapshot
            .items
            .iter()
            .find(|item| item.id == second.id)
            .unwrap();
        assert_eq!(over_budget.title, "item metadata");
        assert!(!over_budget.has_icon_pixmap);
        assert!(over_budget.icon.is_some());
        assert_eq!(over_budget.icon.as_ref().unwrap().retained_bytes(), 0);

        assert!(model.remove(&first.endpoint, Some(&first.owner)));
        assert!(
            model
                .update(&second, queried_with_icon(&second, 20), 4)
                .unwrap()
        );
        let replenished = model.projection(true, false, String::new()).snapshot.items[0].clone();
        assert!(replenished.has_icon_pixmap);
        assert_eq!(replenished.icon.unwrap().retained_bytes(), 4);
    }

    fn queried(lifetime: &Lifetime) -> QueriedItem {
        QueriedItem {
            item: SystemTrayItem {
                id: lifetime.id.clone(),
                protocol_item_id: "item".into(),
                category: "ApplicationStatus".into(),
                title: String::new(),
                status: "Active".into(),
                icon: None,
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
            icon: QueriedIcon::default(),
            attention_icon: QueriedIcon::default(),
            interface: "org.kde.StatusNotifierItem".into(),
        }
    }

    fn queried_with_icon(lifetime: &Lifetime, red: u8) -> QueriedItem {
        let mut queried = queried(lifetime);
        queried.item.title = "item metadata".into();
        queried.item.icon_name = "item-icon".into();
        queried.icon = QueriedIcon {
            name: "item-icon".into(),
            pixmaps: vec![QueriedPixmap {
                width: 1,
                height: 1,
                rgba: vec![red, 2, 3, 255],
            }],
        };
        queried
    }
}
