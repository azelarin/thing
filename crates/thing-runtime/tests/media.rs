use std::fs;

use std::time::{Duration, Instant};

use thing_runtime::{
    Action, Event, MediaAction, MediaPlayer, MediaPosition, MediaSnapshot, Runtime,
};

fn runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn player(id: &str, name: &str, title: &str) -> MediaPlayer {
    MediaPlayer {
        id: id.into(),
        name: name.into(),
        playing: true,
        title: title.into(),
        artist: "Artist".into(),
        art_url: "file:///cover.png".into(),
        identity: "Player".into(),
        desktop_entry: "player".into(),
        can_play_pause: true,
        can_next: true,
        can_previous: false,
        can_stop: true,
        can_seek: true,
        track_id: format!("/org/thing/{id}/track"),
        length_us: Some(200_000_000),
        rate: 1.0,
        position: None,
    }
}

#[test]
fn media_state_is_reactive_and_replaced_together() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local players = services.media.players:get()
                local first = players[1] or { id = '', name = '' }
                return table.concat({
                    tostring(services.media.available:get()),
                    services.media.selected_id:get(),
                    tostring(services.media.playing:get()),
                    services.media.title:get(),
                    services.media.artist:get(),
                    services.media.identity:get(),
                    services.media.desktop_entry:get(),
                    tostring(services.media.can_play_pause:get()),
                    tostring(services.media.can_next:get()),
                    tostring(services.media.can_previous:get()),
                    tostring(services.media.can_stop:get()),
                    tostring(#players),
                    first.id,
                    first.name,
                }, '|')
            end,
        } } }
        "#,
    );

    let player = player("p1", "org.mpris.MediaPlayer2.test", "Track");
    runtime
        .set_media(MediaSnapshot {
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
            players: vec![player],
            error: String::new(),
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|p1|true|Track|Artist|Player|player|true|true|false|true|1|p1|org.mpris.MediaPlayer2.test"
    );

    for invalid in [
        MediaSnapshot {
            available: false,
            title: "bad\0title".into(),
            ..MediaSnapshot::default()
        },
        MediaSnapshot {
            selected_id: "retired".into(),
            ..MediaSnapshot::default()
        },
    ] {
        assert!(runtime.set_media(invalid).is_err());
    }
    assert!(!runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|p1|true|Track|Artist|Player|player|true|true|false|true|1|p1|org.mpris.MediaPlayer2.test"
    );
}

#[test]
fn media_player_records_and_lists_are_read_only() {
    for mutation in [
        "services.media.players:get()[1].title = 'changed'",
        "services.media.players:get()[1] = {}",
    ] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                {mutation}
            end }} }} }}
            "#,
        );
        let (_directory, mut runtime) = runtime(&source);
        let player = player("p1", "org.mpris.MediaPlayer2.test", "Track");
        runtime
            .set_media(MediaSnapshot {
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
                players: vec![player],
                error: String::new(),
            })
            .unwrap();

        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err()
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn malformed_media_player_ids_are_rejected_before_queueing() {
    for argument in ["''", "'bad\\0id'", "7", "{}"] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                services.media.next({argument})
            end }} }} }}
            "#,
        );
        let (_directory, mut runtime) = runtime(&source);
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {argument}"
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn media_controls_queue_ordered_actions() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            services.media.play_pause()
            services.media.next(nil)
            services.media.previous('p2')
            services.media.stop()
        end } } }
        "#,
    );

    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::Media {
                action: MediaAction::PlayPause,
                player: None,
            },
            Action::Media {
                action: MediaAction::Next,
                player: None,
            },
            Action::Media {
                action: MediaAction::Previous,
                player: Some("p2".into()),
            },
            Action::Media {
                action: MediaAction::Stop,
                player: None,
            },
        ]
    );
}

#[test]
fn media_controls_are_event_only_and_read_only() {
    for source in [
        "services.media.play_pause(); return ui.panel {}",
        "return ui.panel { visible = function() services.media.next(); return true end }",
        "services.media.title = 'changed'; return ui.panel {}",
        "services.media.players = {}; return ui.panel {}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}

fn selected(player: MediaPlayer, others: Vec<MediaPlayer>) -> MediaSnapshot {
    let mut players = vec![player.clone()];
    players.extend(others);
    MediaSnapshot {
        available: true,
        selected_id: player.id,
        playing: player.playing,
        title: player.title,
        artist: player.artist,
        art_url: player.art_url,
        identity: player.identity,
        desktop_entry: player.desktop_entry,
        can_play_pause: player.can_play_pause,
        can_next: player.can_next,
        can_previous: player.can_previous,
        can_stop: player.can_stop,
        can_seek: player.can_seek,
        length_us: player.length_us,
        rate: player.rate,
        error: String::new(),
        players,
    }
}

#[test]
fn timeline_fields_mirror_the_selected_player() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local media = services.media
                local first = media.players:get()[1] or {}
                return table.concat({
                    tostring(media.can_seek:get()),
                    tostring(media.length_ms:get()),
                    tostring(media.rate:get()),
                    tostring(first.can_seek),
                    tostring(first.length_ms),
                    tostring(first.rate),
                }, '|')
            end,
        } } }
        "#,
    );
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "false|nil|1|nil|nil|nil"
    );

    let mut player = player("p1", "org.mpris.MediaPlayer2.test", "Track");
    player.rate = 2.0;
    player.length_us = Some(200_000_500);
    runtime
        .set_media(selected(player.clone(), Vec::new()))
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|200000.5|2|true|200000.5|2"
    );

    // the selected mirror must agree with its record.
    let mut snapshot = selected(player.clone(), Vec::new());
    snapshot.rate = 1.0;
    assert!(runtime.set_media(snapshot).is_err());
    for invalid in [
        MediaPlayer {
            rate: 0.0,
            ..player.clone()
        },
        MediaPlayer {
            rate: f64::NAN,
            ..player.clone()
        },
        MediaPlayer {
            rate: f64::INFINITY,
            ..player.clone()
        },
        MediaPlayer {
            rate: -1001.0,
            ..player.clone()
        },
        MediaPlayer {
            length_us: Some(-1),
            ..player.clone()
        },
        MediaPlayer {
            track_id: "bad\0track".into(),
            ..player.clone()
        },
    ] {
        assert!(runtime.set_media(selected(invalid, Vec::new())).is_err());
    }
}

#[test]
fn seek_requests_carry_the_observed_player_and_track() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            services.media.seek(-5000)
            services.media.set_position(90000.4, 'p2')
            services.media.seek(1.5, 'retired')
        end } } }
        "#,
    );
    runtime
        .set_media(selected(
            player("p1", "org.mpris.MediaPlayer2.one", "One"),
            vec![player("p2", "org.mpris.MediaPlayer2.two", "Two")],
        ))
        .unwrap();
    runtime.refresh().unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::Media {
                action: MediaAction::Seek {
                    offset_us: -5_000_000,
                    track: "/org/thing/p1/track".into(),
                },
                player: Some("p1".into()),
            },
            Action::Media {
                action: MediaAction::SetPosition {
                    position_us: 90_000_400,
                    track: "/org/thing/p2/track".into(),
                },
                player: Some("p2".into()),
            },
            // an unknown player has no observed track; the service rejects it.
            Action::Media {
                action: MediaAction::Seek {
                    offset_us: 1500,
                    track: String::new(),
                },
                player: Some("retired".into()),
            },
        ]
    );
}

#[test]
fn malformed_seek_arguments_are_rejected_before_queueing() {
    for call in [
        "services.media.seek(0 / 0)",
        "services.media.seek(math.huge)",
        "services.media.seek(2e15)",
        "services.media.set_position(-1)",
        "services.media.set_position(10, '')",
        "services.media.set_position()",
    ] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                {call}
            end }} }} }}
            "#,
        );
        let (_directory, mut runtime) = runtime(&source);
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {call}"
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn position_estimates_advance_only_while_playing() {
    let (_directory, mut runtime) = runtime(
        r#"
        local reported = ui.state('')
        return ui.panel { children = { ui.button {
            on_click = function()
                local function show(value)
                    return if value then string.format('%.0f', value) else 'nil'
                end
                reported:set(table.concat({
                    show(services.media.position()),
                    show(services.media.position('p2')),
                    show(services.media.position('missing')),
                }, '|'))
            end,
            children = { ui.text { text = function() return reported:get() end } },
        } } }
        "#,
    );
    let sampled = Instant::now() - Duration::from_secs(10);
    let mut playing = player("p1", "org.mpris.MediaPlayer2.one", "One");
    playing.rate = 2.0;
    playing.position = Some(MediaPosition {
        position_us: 50_000_000,
        observed_at: sampled,
    });
    let mut paused = player("p2", "org.mpris.MediaPlayer2.two", "Two");
    paused.playing = false;
    paused.position = Some(MediaPosition {
        position_us: 50_000_000,
        observed_at: sampled,
    });
    runtime
        .set_media(selected(playing.clone(), vec![paused]))
        .unwrap();
    runtime.refresh().unwrap();
    let button = runtime.root().unwrap().children[0].id;
    runtime.dispatch(button, Event::Click).unwrap();
    let text = runtime.root().unwrap().children[0].children[0].text.clone();
    let parts: Vec<&str> = text.split('|').collect();
    // ten seconds at 2x after 50 s; allow for the time this test takes.
    let estimate: f64 = parts[0].parse().unwrap();
    assert!((70_000.0..71_000.0).contains(&estimate), "{text}");
    assert_eq!(parts[1..], ["50000", "nil"]);

    // estimates stop at the track length.
    assert_eq!(
        MediaPlayer {
            length_us: Some(60_000_000),
            ..playing
        }
        .position_at(sampled + Duration::from_secs(10)),
        Some(60_000_000)
    );
}

#[test]
fn reverse_playback_rates_are_preserved_and_estimated_backwards() {
    let (_directory, mut runtime) = runtime("return ui.panel {}");
    let sampled = Instant::now();
    let mut reverse = player("reverse", "org.mpris.MediaPlayer2.reverse", "Reverse");
    reverse.rate = -2.0;
    reverse.position = Some(MediaPosition {
        position_us: 50_000_000,
        observed_at: sampled,
    });
    runtime
        .set_media(selected(reverse.clone(), Vec::new()))
        .unwrap();
    assert_eq!(
        reverse.position_at(sampled + Duration::from_secs(10)),
        Some(30_000_000)
    );
    assert_eq!(
        reverse.position_at(sampled + Duration::from_secs(30)),
        Some(0)
    );
}

#[test]
fn position_samples_invalidate_reactive_readers() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function() return tostring(services.media.position()) end,
        } } }
        "#,
    );
    let mut paused = player("p1", "org.mpris.MediaPlayer2.one", "One");
    paused.playing = false;
    for position_us in [30_000_000, 90_000_000] {
        paused.position = Some(MediaPosition {
            position_us,
            observed_at: Instant::now(),
        });
        runtime
            .set_media(selected(paused.clone(), Vec::new()))
            .unwrap();
        assert!(runtime.refresh().unwrap());
        assert_eq!(
            runtime.root().unwrap().children[0].text,
            (position_us / 1000).to_string()
        );
    }
}
