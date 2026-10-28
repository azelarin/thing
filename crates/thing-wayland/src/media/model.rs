use std::collections::BTreeMap;

use thing_runtime::{MediaAction, MediaPlayer, MediaSnapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Player {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub art_url: String,
    pub identity: String,
    pub desktop_entry: String,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_control: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub id: String,
    pub name: String,
    pub owner: String,
    toggle: Option<PlaybackMethod>,
    can_next: bool,
    can_previous: bool,
    can_stop: bool,
}

pub(super) struct Projection {
    pub snapshot: MediaSnapshot,
    pub selected: Option<Target>,
    pub targets: BTreeMap<String, Target>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackMethod {
    Play,
    Pause,
    Next,
    Previous,
    Stop,
}

impl Target {
    pub fn method(&self, action: MediaAction) -> Option<PlaybackMethod> {
        match action {
            MediaAction::PlayPause => self.toggle,
            MediaAction::Next => self.can_next.then_some(PlaybackMethod::Next),
            MediaAction::Previous => self.can_previous.then_some(PlaybackMethod::Previous),
            MediaAction::Stop => self.can_stop.then_some(PlaybackMethod::Stop),
        }
    }
}

#[derive(Default)]
pub(super) struct Model {
    players: BTreeMap<String, Player>,
    recent: Option<String>,
}

impl Model {
    #[cfg(test)]
    pub fn initialize(&mut self, players: impl IntoIterator<Item = Player>) {
        self.players = players
            .into_iter()
            .map(|player| (player.name.clone(), player))
            .collect();
        self.recent = None;
    }

    pub fn discover(&mut self, player: Player) {
        self.players.insert(player.name.clone(), player);
    }

    pub fn update(&mut self, player: Player) {
        let began_playing = player.playing
            && self
                .players
                .get(&player.name)
                .is_none_or(|previous| previous.id != player.id || !previous.playing);
        if began_playing {
            self.recent = Some(player.name.clone());
        }
        self.players.insert(player.name.clone(), player);
    }

    pub fn remove(&mut self, name: &str, owner: &str) {
        if self
            .players
            .get(name)
            .is_some_and(|player| player.owner == owner)
        {
            self.players.remove(name);
            if self.recent.as_deref() == Some(name) {
                self.recent = None;
            }
        }
    }

    pub fn player(&self, name: &str) -> Option<&Player> {
        self.players.get(name)
    }

    pub fn snapshot(&self) -> Projection {
        let selected_name = self.selected().map(|player| player.name.as_str());
        let mut players = Vec::with_capacity(self.players.len());
        let mut selected_player = None;
        let mut selected = None;
        let mut targets = BTreeMap::new();
        for player in self.players.values() {
            let (record, target) = project(player);
            if selected_name == Some(player.name.as_str()) {
                selected_player = Some(record.clone());
                selected = Some(target.clone());
            }
            targets.insert(target.id.clone(), target);
            players.push(record);
        }
        let Some(player) = selected_player else {
            return Projection {
                snapshot: MediaSnapshot {
                    players,
                    ..MediaSnapshot::default()
                },
                selected,
                targets,
            };
        };
        let snapshot = MediaSnapshot {
            available: true,
            selected_id: player.id.clone(),
            playing: player.playing,
            title: player.title.clone(),
            artist: player.artist.clone(),
            art_url: player.art_url.clone(),
            identity: player.identity.clone(),
            desktop_entry: player.desktop_entry.clone(),
            can_play_pause: player.can_play_pause,
            can_next: player.can_next,
            can_previous: player.can_previous,
            can_stop: player.can_stop,
            error: String::new(),
            players,
        };
        Projection {
            snapshot,
            selected,
            targets,
        }
    }

    fn selected(&self) -> Option<&Player> {
        self.recent
            .as_deref()
            .and_then(|name| self.players.get(name))
            .filter(|player| player.playing)
            .or_else(|| self.players.values().find(|player| player.playing))
            .or_else(|| {
                self.recent
                    .as_deref()
                    .and_then(|name| self.players.get(name))
            })
            .or_else(|| self.players.values().next())
    }
}

fn project(player: &Player) -> (MediaPlayer, Target) {
    let toggle = if !player.can_control {
        None
    } else if player.playing && player.can_pause {
        Some(PlaybackMethod::Pause)
    } else if !player.playing && player.can_play {
        Some(PlaybackMethod::Play)
    } else {
        None
    };
    let record = MediaPlayer {
        id: player.id.clone(),
        name: player.name.clone(),
        playing: player.playing,
        title: player.title.clone(),
        artist: player.artist.clone(),
        art_url: player.art_url.clone(),
        identity: player.identity.clone(),
        desktop_entry: player.desktop_entry.clone(),
        can_play_pause: toggle.is_some(),
        can_next: player.can_control && player.can_next,
        can_previous: player.can_control && player.can_previous,
        can_stop: player.can_control,
    };
    let target = Target {
        id: player.id.clone(),
        name: player.name.clone(),
        owner: player.owner.clone(),
        toggle,
        can_next: record.can_next,
        can_previous: record.can_previous,
        can_stop: record.can_stop,
    };
    (record, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_player_returns_when_no_player_is_active() {
        let mut model = Model::default();
        model.initialize([player("first", true), player("second", false)]);
        assert_eq!(model.snapshot().selected.unwrap().name, "first");

        model.update(player("second", true));
        assert_eq!(model.snapshot().selected.unwrap().name, "second");
        model.update(player("second", false));
        assert_eq!(model.snapshot().selected.unwrap().name, "first");
        model.update(player("first", false));
        assert_eq!(model.snapshot().selected.unwrap().name, "second");
    }

    #[test]
    fn owner_replacement_cannot_be_removed_by_a_stale_event() {
        let mut model = Model::default();
        model.initialize([player("browser", false)]);
        let mut replacement = player("browser", true);
        replacement.id = "replacement".into();
        replacement.owner = ":1.20".into();
        model.update(replacement);

        model.remove("browser", ":1.10");
        let selected = model.snapshot().selected.unwrap();
        assert_eq!(selected.id, "replacement");
        assert_eq!(selected.owner, ":1.20");
        model.remove("browser", ":1.20");
        assert!(!model.snapshot().snapshot.available);
    }

    #[test]
    fn playback_capability_depends_on_current_status() {
        let mut current = player("player", true);
        current.can_play = false;
        current.can_pause = true;
        let mut model = Model::default();
        model.initialize([current.clone()]);
        let projection = model.snapshot();
        assert!(projection.snapshot.can_play_pause);
        assert_eq!(
            projection.selected.unwrap().method(MediaAction::PlayPause),
            Some(PlaybackMethod::Pause)
        );

        current.playing = false;
        model.update(current);
        let projection = model.snapshot();
        assert!(!projection.snapshot.can_play_pause);
        assert_eq!(
            projection.selected.unwrap().method(MediaAction::PlayPause),
            None
        );
    }

    #[test]
    fn snapshot_exposes_every_player_without_changing_selection() {
        let mut model = Model::default();
        model.initialize([player("first", true), player("second", false)]);

        let projection = model.snapshot();
        assert_eq!(projection.snapshot.selected_id, "id-first");
        assert_eq!(
            projection
                .snapshot
                .players
                .iter()
                .map(|player| player.id.as_str())
                .collect::<Vec<_>>(),
            ["id-first", "id-second"]
        );
        assert_eq!(projection.targets["id-second"].name, "second");
    }

    fn player(name: &str, playing: bool) -> Player {
        Player {
            id: format!("id-{name}"),
            name: name.into(),
            owner: ":1.10".into(),
            playing,
            title: format!("{name} title"),
            artist: format!("{name} artist"),
            art_url: String::new(),
            identity: name.into(),
            desktop_entry: name.into(),
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            can_control: true,
        }
    }
}
