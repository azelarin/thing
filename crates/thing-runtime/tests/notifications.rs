use std::fs;

use thing_runtime::{
    Action, Event, Notification, NotificationButton, NotificationSnapshot, NotificationUrgency,
    Runtime,
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
        created_at_ms: 20,
        updated_at_ms: 20,
        expires_at_ms: Some(5_020),
        category: "email.arrived".into(),
        desktop_entry: "mail-client".into(),
        image_path: String::new(),
        resident: false,
        transient: false,
        action_icons: false,
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
