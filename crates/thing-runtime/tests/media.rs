use std::fs;

use thing_runtime::{Action, Event, MediaAction, MediaPlayer, MediaSnapshot, Runtime};

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
