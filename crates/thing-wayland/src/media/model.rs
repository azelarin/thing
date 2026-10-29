use std::collections::BTreeMap;

use thing_runtime::{MediaAction, MediaPlayer, MediaPosition, MediaSnapshot};

#[derive(Clone, Debug, PartialEq)]
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
    pub can_seek: bool,
    pub track_id: String,
    pub length_us: Option<i64>,
    pub rate: f64,
    pub position: Option<MediaPosition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub track_id: String,
    toggle: Option<PlaybackMethod>,
    can_next: bool,
    can_previous: bool,
    can_stop: bool,
    can_seek: bool,
}

pub(super) struct Projection {
    pub snapshot: MediaSnapshot,
    pub selected: Option<Target>,
    pub targets: BTreeMap<String, Target>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlaybackMethod {
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    Seek(i64),
    SetPosition { track_id: String, position_us: i64 },
}

/// why a target cannot perform a requested action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    Unsupported,
    TrackChanged,
}

impl Target {
    pub fn method(&self, action: &MediaAction) -> Result<PlaybackMethod, Refusal> {
        let supported = |method: Option<PlaybackMethod>| method.ok_or(Refusal::Unsupported);
        match action {
            MediaAction::PlayPause => supported(self.toggle.clone()),
            MediaAction::Next => supported(self.can_next.then_some(PlaybackMethod::Next)),
            MediaAction::Previous => {
                supported(self.can_previous.then_some(PlaybackMethod::Previous))
            }
            MediaAction::Stop => supported(self.can_stop.then_some(PlaybackMethod::Stop)),
            MediaAction::Seek { offset_us, track } => {
                if !self.can_seek {
                    return Err(Refusal::Unsupported);
                }
                if *track != self.track_id {
                    return Err(Refusal::TrackChanged);
                }
                Ok(PlaybackMethod::Seek(*offset_us))
            }
            MediaAction::SetPosition { position_us, track } => {
                // absolute positions are addressed to a track id, so a player
                // that reports none cannot accept one.
                if !self.can_seek || self.track_id.is_empty() {
                    return Err(Refusal::Unsupported);
                }
                if *track != self.track_id {
                    return Err(Refusal::TrackChanged);
                }
                Ok(PlaybackMethod::SetPosition {
                    track_id: self.track_id.clone(),
                    position_us: *position_us,
                })
            }
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
            can_seek: player.can_seek,
            length_us: player.length_us,
            rate: player.rate,
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
        can_seek: player.can_control && player.can_seek,
        track_id: player.track_id.clone(),
        length_us: player.length_us,
        rate: player.rate,
        position: player.position,
    };
    let target = Target {
        id: player.id.clone(),
        name: player.name.clone(),
        owner: player.owner.clone(),
        track_id: player.track_id.clone(),
        toggle,
        can_next: record.can_next,
        can_previous: record.can_previous,
        can_stop: record.can_stop,
        can_seek: record.can_seek,
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
            projection.selected.unwrap().method(&MediaAction::PlayPause),
            Ok(PlaybackMethod::Pause)
        );

        current.playing = false;
        model.update(current);
        let projection = model.snapshot();
        assert!(!projection.snapshot.can_play_pause);
        assert_eq!(
            projection.selected.unwrap().method(&MediaAction::PlayPause),
            Err(Refusal::Unsupported)
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

    #[test]
    fn seeks_require_capability_and_the_observed_track() {
        let mut model = Model::default();
        model.initialize([player("player", true)]);
        let target = model.snapshot().selected.unwrap();
        let track = target.track_id.clone();
        assert_eq!(
            target.method(&MediaAction::Seek {
                offset_us: 5,
                track: track.clone(),
            }),
            Ok(PlaybackMethod::Seek(5))
        );
        assert_eq!(
            target.method(&MediaAction::SetPosition {
                position_us: 7,
                track: track.clone(),
            }),
            Ok(PlaybackMethod::SetPosition {
                track_id: track.clone(),
                position_us: 7,
            })
        );
        assert_eq!(
            target.method(&MediaAction::Seek {
                offset_us: 5,
                track: "/org/thing/other".into(),
            }),
            Err(Refusal::TrackChanged)
        );

        let mut untracked = player("untracked", true);
        untracked.track_id.clear();
        let mut uncontrolled = player("uncontrolled", true);
        uncontrolled.can_control = false;
        model.initialize([untracked, uncontrolled]);
        let projection = model.snapshot();
        let untracked = &projection.targets["id-untracked"];
        // a relative seek needs no track id, but an absolute position does.
        assert_eq!(
            untracked.method(&MediaAction::Seek {
                offset_us: 5,
                track: String::new(),
            }),
            Ok(PlaybackMethod::Seek(5))
        );
        assert_eq!(
            untracked.method(&MediaAction::SetPosition {
                position_us: 5,
                track: String::new(),
            }),
            Err(Refusal::Unsupported)
        );
        let uncontrolled = &projection.targets["id-uncontrolled"];
        assert!(!projection.snapshot.players[0].can_seek);
        assert_eq!(
            uncontrolled.method(&MediaAction::Seek {
                offset_us: 5,
                track: uncontrolled.track_id.clone(),
            }),
            Err(Refusal::Unsupported)
        );
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
            can_seek: true,
            track_id: format!("/org/thing/{name}/1"),
            length_us: Some(180_000_000),
            rate: 1.0,
            position: None,
        }
    }
}
