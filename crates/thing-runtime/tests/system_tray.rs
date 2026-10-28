use std::fs;

use thing_core::{ImageCandidate, ImagePixmap, ImageSource};
use thing_runtime::{Action, Event, Runtime, SystemTrayAction, SystemTrayItem, SystemTraySnapshot};

fn runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn item(id: &str) -> SystemTrayItem {
    SystemTrayItem {
        id: id.into(),
        protocol_item_id: "chat".into(),
        category: "Communications".into(),
        title: "Chat".into(),
        status: "NeedsAttention".into(),
        icon: Some(image_source(id, 10)),
        icon_name: "chat".into(),
        attention_icon_name: "chat-attention".into(),
        overlay_icon_name: "chat-unread".into(),
        tooltip_icon_name: "dialog-information".into(),
        tooltip_title: "new message".into(),
        tooltip_text: "one unread conversation".into(),
        item_is_menu: false,
        has_menu: true,
        has_icon_pixmap: true,
        has_attention_icon_pixmap: false,
        has_overlay_icon_pixmap: true,
        has_tooltip_icon_pixmap: false,
    }
}

fn image_source(id: &str, red: u8) -> ImageSource {
    ImageSource::new(
        format!("{id}-image{red}"),
        vec![
            ImageCandidate::new(
                Some("chat-attention".into()),
                vec![ImagePixmap::new(1, 1, vec![red, 20, 30, 255]).unwrap()],
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn system_tray_state_is_reactive_and_replaced_together() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local items = services.system_tray.items:get()
                local item = items[1] or {
                    id = '', protocol_item_id = '', category = '', title = '', status = '',
                    icon_name = '', attention_icon_name = '', overlay_icon_name = '',
                    tooltip_icon_name = '', tooltip_title = '', tooltip_text = '',
                    item_is_menu = false, has_menu = false, has_icon_pixmap = false,
                    has_attention_icon_pixmap = false, has_overlay_icon_pixmap = false,
                    has_tooltip_icon_pixmap = false,
                }
                return table.concat({
                    tostring(services.system_tray.available:get()),
                    tostring(services.system_tray.watcher_owned:get()),
                    services.system_tray.error:get(),
                    tostring(#items), item.id, item.protocol_item_id, item.category, item.title,
                    item.status, item.icon_name, item.attention_icon_name,
                    item.overlay_icon_name, item.tooltip_icon_name, item.tooltip_title,
                    item.tooltip_text, tostring(item.item_is_menu), tostring(item.has_menu),
                    tostring(item.has_icon_pixmap), tostring(item.has_attention_icon_pixmap),
                    tostring(item.has_overlay_icon_pixmap), tostring(item.has_tooltip_icon_pixmap),
                }, '|')
            end,
        } } }
        "#,
    );

    runtime
        .set_system_tray(SystemTraySnapshot {
            available: true,
            watcher_owned: true,
            error: String::new(),
            items: vec![item("tray1")],
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|true||1|tray1|chat|Communications|Chat|NeedsAttention|chat|chat-attention|chat-unread|dialog-information|new message|one unread conversation|false|true|true|false|true|false"
    );

    let oversized_icon = ImageSource::new(
        "tray2-image1".into(),
        vec![
            ImageCandidate::new(
                None,
                vec![ImagePixmap::new(257, 257, vec![0; 257 * 257 * 4]).unwrap()],
            )
            .unwrap(),
        ],
    )
    .unwrap();
    for invalid in [
        SystemTraySnapshot {
            available: false,
            items: vec![item("tray2")],
            ..SystemTraySnapshot::default()
        },
        SystemTraySnapshot {
            available: true,
            items: vec![item("duplicate"), item("duplicate")],
            ..SystemTraySnapshot::default()
        },
        SystemTraySnapshot {
            available: true,
            items: vec![SystemTrayItem {
                tooltip_text: "bad\0tooltip".into(),
                ..item("tray2")
            }],
            ..SystemTraySnapshot::default()
        },
        SystemTraySnapshot {
            available: true,
            items: vec![SystemTrayItem {
                icon: Some(oversized_icon),
                ..item("tray2")
            }],
            ..SystemTraySnapshot::default()
        },
    ] {
        assert!(runtime.set_system_tray(invalid).is_err());
    }
    assert!(!runtime.refresh().unwrap());
}

#[test]
fn system_tray_records_and_lists_are_read_only() {
    for mutation in [
        "services.system_tray.items:get()[1].title = 'changed'",
        "services.system_tray.items:get()[1] = {}",
        "services.system_tray.items:get()[1].icon.value = 'changed'",
    ] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                {mutation}
            end }} }} }}
            "#,
        );
        let (_directory, mut runtime) = runtime(&source);
        runtime
            .set_system_tray(SystemTraySnapshot {
                available: true,
                items: vec![item("tray1")],
                ..SystemTraySnapshot::default()
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
fn system_tray_image_sources_are_opaque_and_reactive() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.image {
            source = function()
                local items = services.system_tray.items:get()
                return if #items == 0 then nil else items[1].icon
            end,
        } } }
        "#,
    );
    runtime
        .set_system_tray(SystemTraySnapshot {
            available: true,
            items: vec![item("tray1")],
            ..SystemTraySnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0]
            .image
            .memory
            .as_ref()
            .unwrap()
            .id(),
        "tray1-image10"
    );

    let mut changed = item("tray1");
    changed.icon = Some(image_source("tray1", 40));
    runtime
        .set_system_tray(SystemTraySnapshot {
            available: true,
            items: vec![changed],
            ..SystemTraySnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0]
            .image
            .memory
            .as_ref()
            .unwrap()
            .id(),
        "tray1-image40"
    );

    let invalid = tempfile::tempdir().unwrap();
    let path = invalid.path().join("invalid.luau");
    fs::write(
        &path,
        "return ui.panel { children = { ui.image { source = services.system_tray.items } } }",
    )
    .unwrap();
    let error = Runtime::load(path).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("source must be a string, image source, or nil")
    );
}

#[test]
fn system_tray_actions_are_ordered_and_lifetime_targeted() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            services.system_tray.activate('tray1')
            services.system_tray.secondary_activate('tray2')
            services.system_tray.context_menu('tray3')
        end } } }
        "#,
    );

    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::SystemTray {
                action: SystemTrayAction::Activate,
                item: "tray1".into(),
            },
            Action::SystemTray {
                action: SystemTrayAction::SecondaryActivate,
                item: "tray2".into(),
            },
            Action::SystemTray {
                action: SystemTrayAction::ContextMenu,
                item: "tray3".into(),
            },
        ]
    );
}

#[test]
fn malformed_system_tray_ids_are_rejected_before_queueing() {
    for argument in ["''", "'bad\\0id'", "7", "{}", "nil"] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                services.system_tray.activate({argument})
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
fn system_tray_actions_are_event_only_and_service_is_read_only() {
    for source in [
        "services.system_tray.activate('tray1'); return ui.panel {}",
        "return ui.panel { visible = function() services.system_tray.context_menu('tray1'); return true end }",
        "services.system_tray.available = true; return ui.panel {}",
        "services.system_tray.items = {}; return ui.panel {}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}
