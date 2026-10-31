use std::fs;

use thing_core::{ImageCandidate, ImagePixmap, ImageSource};
use thing_runtime::{
    Action, Event, Notification, NotificationButton, NotificationCloseReason, NotificationSnapshot,
    NotificationUrgency, Runtime,
};

fn load_runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn notification(key: &str, id: u32, summary: &str) -> Notification {
    Notification {
        key: key.into(),
        id,
        revision: 1,
        app_name: "Mail".into(),
        app_icon: "mail-client".into(),
        summary: summary.into(),
        body: "One new message".into(),
        actions: vec![NotificationButton {
            key: "default".into(),
            label: "Open".into(),
        }],
        urgency: NotificationUrgency::Normal,
        raw_expire_timeout_ms: -1,
        effective_timeout_ms: Some(5_000),
        expiry_paused: false,
        paused_remaining_ms: None,
        time_origin: std::time::Instant::now(),
        created_at_ms: 20,
        updated_at_ms: 20,
        expires_at_ms: Some(5_020),
        category: "email.arrived".into(),
        desktop_entry: "mail-client".into(),
        image_path: String::new(),
        resident: false,
        transient: false,
        action_icons: false,
        can_reply: false,
        reply_placeholder: String::new(),
        image: None,
        closed: None,
    }
}

#[test]
fn notification_state_is_reactive_and_records_are_immutable() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local active = services.notifications.active:get()
                local first = active[1] or {
                    key = '', id = 0, revision = 0, summary = '', urgency = '',
                    urgency_code = 0, actions = {}, app_name = '', app_icon = '', body = '',
                    raw_expire_timeout_ms = 0, effective_timeout_ms = 0,
                    created_at_ms = 0, updated_at_ms = 0, expires_at_ms = 0,
                    category = '', desktop_entry = '', image_path = '',
                    resident = false, transient = false, action_icons = false,
                }
                local action = first.actions[1] or { key = '', label = '' }
                return table.concat({
                    tostring(services.notifications.owned:get()),
                    services.notifications.error:get(),
                    tostring(#active),
                    first.key,
                    tostring(first.id),
                    tostring(first.revision),
                    first.app_name,
                    first.app_icon,
                    first.summary,
                    first.body,
                    first.urgency,
                    tostring(first.urgency_code),
                    tostring(first.raw_expire_timeout_ms),
                    tostring(first.effective_timeout_ms),
                    tostring(first.created_at_ms),
                    tostring(first.updated_at_ms),
                    tostring(first.expires_at_ms),
                    first.category,
                    first.desktop_entry,
                    first.image_path,
                    tostring(first.resident),
                    tostring(first.transient),
                    tostring(first.action_icons),
                    action.key,
                    action.label,
                }, '|')
            end,
        } } }
        "#,
    );

    runtime
        .set_notifications(NotificationSnapshot {
            history: Vec::new(),
            owned: true,
            error: String::new(),
            active: vec![notification("9007199254740993", 7, "Hello")],
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true||1|9007199254740993|7|1|Mail|mail-client|Hello|One new message|normal|1|-1|5000|20|20|5020|email.arrived|mail-client||false|false|false|default|Open"
    );

    let mut updated = notification("9007199254740993", 7, "Updated");
    updated.revision = 2;
    updated.updated_at_ms = 40;
    updated.expires_at_ms = Some(5_040);
    runtime
        .set_notifications(NotificationSnapshot {
            history: Vec::new(),
            owned: true,
            error: String::new(),
            active: vec![updated],
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert!(
        runtime.root().unwrap().children[0]
            .text
            .contains("|2|Mail|mail-client|Updated|")
    );

    for source in [
        "active[1].summary = 'changed'",
        "active[1].actions[1].label = 'changed'",
        "active[1].actions[1] = {}",
        "active[1] = {}",
    ] {
        let script = format!(
            "return ui.panel {{ children = function() local active = services.notifications.active:get(); if active[1] then {source} end; return {{}} end }}"
        );
        let (_directory, mut runtime) = load_runtime(&script);
        runtime
            .set_notifications(NotificationSnapshot {
                history: Vec::new(),
                owned: true,
                error: String::new(),
                active: vec![notification("1", 1, "Hello")],
            })
            .unwrap();
        assert!(runtime.refresh().is_err(), "accepted {source}");
    }
}

#[test]
fn notification_actions_are_ordered_and_event_only() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            services.notifications.dismiss('9007199254740993')
            services.notifications.invoke('42', 'default')
        end } } }
        "#,
    );
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::DismissNotification {
                key: 9_007_199_254_740_993,
            },
            Action::InvokeNotification {
                key: 42,
                action: "default".into(),
            },
        ]
    );

    for source in [
        "services.notifications.dismiss('1'); return ui.panel {}",
        "return ui.panel { visible = function() services.notifications.invoke('1', 'default'); return true end }",
        "services.notifications.owned = true; return ui.panel {}",
        "services.notifications.active = {}; return ui.panel {}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}

#[test]
fn malformed_notification_state_and_actions_are_rejected_atomically() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local active = services.notifications.active:get()
                return active[1] and active[1].summary or 'empty'
            end,
        } } }
        "#,
    );
    let valid = NotificationSnapshot {
        history: Vec::new(),
        owned: true,
        error: String::new(),
        active: vec![notification("1", 1, "Valid")],
    };
    runtime.set_notifications(valid.clone()).unwrap();
    assert!(runtime.refresh().unwrap());

    let mut invalid = valid;
    invalid.active[0].actions.push(NotificationButton {
        key: "default".into(),
        label: "Duplicate".into(),
    });
    assert!(runtime.set_notifications(invalid).is_err());
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "Valid");

    let mut oversized = notification("2", 2, "Oversized");
    oversized.app_name = "x".repeat(64 * 1024);
    oversized.app_icon = "x".repeat(64 * 1024);
    oversized.summary = "x".repeat(64 * 1024);
    oversized.body = "x".repeat(64 * 1024);
    oversized.category = "x".repeat(64 * 1024);
    oversized.desktop_entry = "x".repeat(64 * 1024);
    oversized.image_path = "x".repeat(64 * 1024);
    oversized.actions[0].label = "x".repeat(64 * 1024);
    oversized.actions.push(NotificationButton {
        key: "second".into(),
        label: "x".repeat(64 * 1024),
    });
    assert!(
        runtime
            .set_notifications(NotificationSnapshot {
                history: Vec::new(),
                owned: true,
                error: String::new(),
                active: vec![oversized],
            })
            .is_err()
    );
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "Valid");

    let active = (1..=140)
        .map(|id| {
            let mut notification = notification(&id.to_string(), id, "Large");
            notification.body = "x".repeat(60 * 1024);
            notification
        })
        .collect();
    assert!(
        runtime
            .set_notifications(NotificationSnapshot {
                history: Vec::new(),
                owned: true,
                error: String::new(),
                active,
            })
            .is_err()
    );
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "Valid");

    for argument in ["''", "'01'", "'0'", "'bad'", "7", "{}"] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.button {{ on_click = function() services.notifications.dismiss({argument}) end }} }} }}"
        );
        let (_directory, mut runtime) = load_runtime(&source);
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {argument}"
        );
        assert!(runtime.take_actions().is_empty());
    }
}

fn closed(
    mut notification: Notification,
    reason: NotificationCloseReason,
    at_ms: u64,
) -> Notification {
    notification.closed = Some((reason, at_ms));
    notification
}

#[test]
fn history_replies_and_images_are_exposed_to_luau() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local notifications = services.notifications
                local active = notifications.active:get()[1]
                if not active then
                    return 'none'
                end
                local parts = {
                    tostring(active.can_reply),
                    active.reply_placeholder,
                    tostring(active.image ~= nil),
                    tostring(active.closed_reason),
                }
                for _, record in notifications.history:get() do
                    table.insert(parts, record.key .. ':' .. record.closed_reason .. ':' .. record.closed_at_ms)
                end
                return table.concat(parts, '|')
            end,
        } } }
        "#,
    );
    let mut chat = notification("9", 4, "Ada");
    chat.can_reply = true;
    chat.reply_placeholder = "Reply to Ada".into();
    chat.image = Some(
        ImageSource::new(
            "notification-image:9:1".into(),
            vec![
                ImageCandidate::new(
                    None,
                    vec![ImagePixmap::new(1, 1, vec![0, 0, 0, 255]).unwrap()],
                )
                .unwrap(),
            ],
        )
        .unwrap(),
    );
    // closed protocol ids may be reused, so the history may repeat one.
    let history = vec![
        closed(
            notification("8", 4, "earlier"),
            NotificationCloseReason::Expired,
            900,
        ),
        closed(
            notification("7", 4, "older"),
            NotificationCloseReason::ClosedByApplication,
            800,
        ),
    ];
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            error: String::new(),
            active: vec![chat],
            history,
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|Reply to Ada|true|nil|8:expired:900|7:closed_by_application:800"
    );
}

#[test]
fn history_snapshots_are_validated_as_a_whole() {
    let (_directory, mut runtime) = load_runtime("return ui.panel {}");
    let active = notification("1", 1, "active");
    let record = closed(
        notification("2", 2, "closed"),
        NotificationCloseReason::Dismissed,
        50,
    );
    for (active, history) in [
        // an active record must not be closed, and a history record must be.
        (
            vec![closed(
                active.clone(),
                NotificationCloseReason::Dismissed,
                1,
            )],
            vec![],
        ),
        (vec![], vec![active.clone()]),
        // keys are unique across both lists.
        (
            vec![active.clone()],
            vec![closed(active.clone(), NotificationCloseReason::Expired, 9)],
        ),
    ] {
        assert!(
            runtime
                .set_notifications(NotificationSnapshot {
                    owned: true,
                    error: String::new(),
                    active,
                    history,
                })
                .is_err()
        );
    }
    assert!(
        runtime
            .set_notifications(NotificationSnapshot {
                owned: false,
                error: String::new(),
                active: vec![],
                history: vec![record.clone()],
            })
            .is_err()
    );
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            error: String::new(),
            active: vec![active],
            history: vec![record],
        })
        .unwrap();
}

#[test]
fn replies_and_history_controls_queue_actions() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            services.notifications.reply('9', 'on my way')
            services.notifications.forget('8')
            services.notifications.clear_history()
        end } } }
        "#,
    );
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        [
            Action::ReplyNotification {
                key: 9,
                text: "on my way".into(),
            },
            Action::ForgetNotification { key: 8 },
            Action::ClearNotificationHistory,
        ]
    );
    for call in [
        "services.notifications.reply('9', '')",
        "services.notifications.reply('9', 'a\\0b')",
        "services.notifications.reply('x', 'hi')",
        "services.notifications.forget('0')",
    ] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.button {{ on_click = function() {call} end }} }} }}"
        );
        let (_directory, mut rejected) = load_runtime(&source);
        assert!(
            rejected
                .dispatch(rejected.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {call}"
        );
        assert!(rejected.take_actions().is_empty());
    }
}

#[test]
fn history_record_limit_is_validated_before_replacing_the_snapshot() {
    let (_directory, mut runtime) = load_runtime(
        "return ui.panel { children = { ui.text { text = function() return tostring(#services.notifications.history:get()) end } } }",
    );
    let mut history: Vec<_> = (1..=129)
        .map(|id| {
            closed(
                notification(&id.to_string(), id, "closed"),
                NotificationCloseReason::Dismissed,
                50,
            )
        })
        .collect();
    let extra = history.pop().unwrap();
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            error: String::new(),
            active: vec![],
            history: history.clone(),
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "128");
    history.push(extra);
    assert!(
        runtime
            .set_notifications(NotificationSnapshot {
                owned: true,
                error: String::new(),
                active: vec![],
                history,
            })
            .is_err()
    );
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "128");
}

#[test]
fn expiry_holds_commit_only_after_an_accepted_event() {
    let (_directory, mut runtime) = load_runtime(
        r#"
        local n = services.notifications
        local revision = ui.state(0)
        return ui.panel { children = {
            ui.button { on_hover = function(inside)
                n.set_expiry_paused('1', inside)
                if inside then assert(n.active:get()[1].expiry_paused == false) end
            end },
            ui.button { on_click = function()
                n.set_expiry_paused('1', false); revision:set(revision:get() + 1)
            end, children = { ui.text { text = function() return tostring(revision:get()) end } } },
            ui.button { on_click = function() n.set_expiry_paused('1', false); error('failed') end },
        } }
    "#,
    );
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            active: vec![notification("1", 1, "mail")],
            ..NotificationSnapshot::default()
        })
        .unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Hover(true))
        .unwrap();
    assert_eq!(runtime.notification_expiry_holds(), vec![1]);
    assert!(runtime.take_actions().is_empty());
    assert!(
        runtime
            .dispatch(runtime.root().unwrap().children[2].id, Event::Click)
            .is_err()
    );
    assert_eq!(runtime.notification_expiry_holds(), vec![1]);
    let release = runtime.root().unwrap().children[1].id;
    // host rejection must also discard controls from a successful callback.
    assert!(
        runtime
            .dispatch_checked(release, Event::Click, |_| Err("rejected".into()))
            .is_err()
    );
    assert_eq!(runtime.notification_expiry_holds(), vec![1]);
    runtime.dispatch(release, Event::Click).unwrap();
    assert!(runtime.notification_expiry_holds().is_empty());
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Hover(true))
        .unwrap();
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            active: vec![],
            ..NotificationSnapshot::default()
        })
        .unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Hover(false))
        .unwrap();
    assert!(runtime.notification_expiry_holds().is_empty());
}

#[test]
fn expiry_controls_check_phase_keys_and_boolean_arguments() {
    for call in [
        "n.set_expiry_paused('1', true)",
        "n.set_expiry_paused('1', false)",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            format!("local n=services.notifications; {call}; return ui.panel {{}}"),
        )
        .unwrap();
        assert!(Runtime::load(&path).is_err());
    }
    for call in [
        "n.set_expiry_paused('1', 'true')",
        "n.set_expiry_paused(1, true)",
        "n.set_expiry_paused('2', true)",
        "n.set_expiry_paused('18446744073709551616', true)",
        "for _=1,129 do n.set_expiry_paused('1', true) end",
    ] {
        let (_directory, mut runtime) = load_runtime(&format!(
            "local n=services.notifications; return ui.panel {{ children = {{ ui.button {{ on_click=function() {call} end }} }} }}"
        ));
        runtime
            .set_notifications(NotificationSnapshot {
                owned: true,
                active: vec![notification("1", 1, "mail")],
                ..NotificationSnapshot::default()
            })
            .unwrap();
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "{call}"
        );
        assert!(runtime.notification_expiry_holds().is_empty());
    }
}

#[test]
fn remaining_time_uses_a_monotonic_origin_and_frozen_pause_sample() {
    use std::time::{Duration, Instant};
    let mut record = notification("1", 1, "mail");
    let origin = Instant::now();
    record.time_origin = origin;
    assert_eq!(record.remaining_at(origin), Some(5000));
    assert_eq!(
        record.remaining_at(origin + Duration::from_millis(1020)),
        Some(4000)
    );
    assert_eq!(
        record.remaining_at(origin + Duration::from_millis(6020)),
        Some(0)
    );
    record.expiry_paused = true;
    record.expires_at_ms = None;
    record.paused_remaining_ms = Some(1234);
    assert_eq!(
        record.remaining_at(origin + Duration::from_secs(100)),
        Some(1234)
    );
    let (_directory, mut runtime) = load_runtime(
        r#"
        return ui.panel { children = { ui.text { text = function()
            local n = services.notifications
            local record = n.active:get()[1]
            if not record then return 'empty' end
            assert(not pcall(function() record.expiry_paused = false end))
            assert(n.remaining('2') == nil)
            return tostring(record.expiry_paused) .. ':' .. tostring(n.remaining(record.key))
        end } } }
    "#,
    );
    runtime
        .set_notifications(NotificationSnapshot {
            owned: true,
            active: vec![record.clone()],
            ..NotificationSnapshot::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "true:1234");
    let mut invalid = record.clone();
    invalid.paused_remaining_ms = Some(5001);
    assert!(
        runtime
            .set_notifications(NotificationSnapshot {
                owned: true,
                active: vec![invalid],
                ..NotificationSnapshot::default()
            })
            .is_err()
    );
    assert!(!runtime.refresh().unwrap());
    record.closed = Some((NotificationCloseReason::Dismissed, 2000));
    assert_eq!(record.remaining_at(origin), None);
}
