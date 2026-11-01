use std::{
    fs,
    time::{Duration, Instant},
};

use super::{
    App, ManagedProcessScopeChange, PanelState, arm_managed_processes, configure_managed_processes,
    configured_panel_size, earliest_deadline, fallback_logical_size, find_automatic_popup,
    find_manual_popup, interactive_path, panel_size, prepare_and_commit_managed_processes,
    project_managed_processes, release_system_tray_consumer, send_managed_process_action,
    send_system_tray_action, send_system_tray_menu_action, valid_dimensions,
    validate_surface_capabilities,
};
use crate::{configuration::Configurations, panel};
use tempfile::tempdir;
use thing_core::{KeyboardMode, Kind, Node, NodeId, PanelEdge, PanelMargins, Scale};
use thing_runtime::{
    Event, ManagedProcessAction, ManagedProcessDefinition, ManagedProcessShutdownSignal,
    ManagedProcessStatus, OutputContext, ServiceSnapshot, SurfaceKey, SurfaceTree,
    SystemTrayAction, SystemTrayMenuAction,
};

#[test]
fn disabled_system_tray_rejects_actions_without_starting_a_worker() {
    assert_eq!(
        send_system_tray_action(None, SystemTrayAction::Activate, "tray1"),
        Err("system tray capability is disabled".into())
    );
    assert_eq!(
        send_system_tray_menu_action(
            None,
            SystemTrayMenuAction::Open,
            "menu1",
            Some("configuration1"),
        ),
        Err("system tray capability is disabled".into())
    );
    assert_eq!(release_system_tray_consumer(None, "configuration1"), Ok(()));
}

#[test]
fn empty_process_scope_does_not_start_the_optional_worker() {
    let event_loop = calloop::EventLoop::<App>::try_new().unwrap();
    let mut manager = None;

    configure_managed_processes(
        &mut manager,
        &event_loop.handle(),
        &[],
        ManagedProcessScopeChange::Replace,
    )
    .unwrap();

    assert!(manager.is_none());
    assert_eq!(
        send_managed_process_action(None, "query", ManagedProcessAction::Stop),
        Err("managed process service is unavailable".into())
    );
}

#[test]
fn process_preflight_reprojects_a_preserved_state_change() {
    let event_loop = calloop::EventLoop::<App>::try_new().unwrap();
    let mut manager = None;
    let definitions = vec![ManagedProcessDefinition {
        key: "query".into(),
        shutdown_signal: ManagedProcessShutdownSignal::Terminate,
        command: None,
    }];
    configure_managed_processes(
        &mut manager,
        &event_loop.handle(),
        &definitions,
        ManagedProcessScopeChange::Replace,
    )
    .unwrap();
    let projected = project_managed_processes(
        manager.as_ref(),
        &definitions,
        ManagedProcessScopeChange::Reconcile,
    )
    .unwrap();
    assert_eq!(projected.states[0].status, ManagedProcessStatus::Idle);
    manager
        .as_ref()
        .unwrap()
        .start_process(
            "query",
            vec!["/thing/preflight-fixture-does-not-exist".into()],
            None,
        )
        .unwrap();

    let mut applied = Vec::new();
    let activation = prepare_and_commit_managed_processes(
        &mut manager,
        &event_loop.handle(),
        &definitions,
        ManagedProcessScopeChange::Reconcile,
        projected,
        |snapshot| {
            applied.push(snapshot);
            Ok(())
        },
    )
    .unwrap();
    arm_managed_processes(manager.as_ref(), activation);

    assert!(!applied.is_empty());
    assert_ne!(
        applied.last().unwrap().states[0].status,
        ManagedProcessStatus::Idle
    );
}

#[test]
fn renderer_retry_can_wake_before_hover_and_configuration_deadlines() {
    let now = Instant::now();
    let retry = now + Duration::from_millis(16);
    let hover = now + Duration::from_millis(50);
    let reload = now + Duration::from_millis(80);

    assert_eq!(
        earliest_deadline(Some(reload), Some(hover), None, [retry]),
        Some(retry)
    );
    assert_eq!(
        earliest_deadline(Some(reload), Some(hover), None, []),
        Some(hover)
    );
    assert_eq!(
        earliest_deadline(Some(reload), None, None, []),
        Some(reload)
    );
    assert_eq!(earliest_deadline(None, None, None, [retry]), Some(retry));
    let launch = now + Duration::from_millis(8);
    assert_eq!(
        earliest_deadline(Some(reload), Some(hover), Some(launch), [retry]),
        Some(launch)
    );
}

#[test]
fn mode_fallback_applies_scale_and_rotation() {
    assert_eq!(
        fallback_logical_size((3840, 2160), 2, false),
        Some((1920, 1080))
    );
    assert_eq!(
        fallback_logical_size((3840, 2160), 2, true),
        Some((1080, 1920))
    );
}

#[test]
fn panel_fill_axis_uses_the_output_size_and_margins() {
    let mut top = Node::new(NodeId(1), Kind::Panel);
    top.style.height = Some(52.0);
    top.panel.margins = PanelMargins {
        right: 6,
        left: 4,
        ..PanelMargins::default()
    };
    assert_eq!(
        panel_size(panel::geometry(&top, None), (1920, 1080)),
        (1910, 52)
    );

    top.style.width = Some(480.0);
    assert_eq!(
        panel_size(panel::geometry(&top, None), (1920, 1080)),
        (480, 52)
    );
}

#[test]
fn zero_configure_dimensions_follow_the_current_panel_request() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    root.panel.edge = PanelEdge::Left;
    root.panel.margins = PanelMargins {
        top: 5,
        bottom: 7,
        ..PanelMargins::default()
    };
    root.style.width = Some(48.0);
    let side = panel::geometry(&root, None);
    assert_eq!(
        configured_panel_size((48, 0), side, (1920, 1080)),
        (48, 1068)
    );

    root.panel.edge = PanelEdge::Top;
    root.style.width = None;
    root.style.height = Some(80.0);
    let resized_top = panel::geometry(&root, None);
    assert_eq!(
        configured_panel_size((0, 0), resized_top, (1920, 1080)),
        (1920, 80)
    );
}

#[test]
fn explicit_exclusivity_differs_from_an_equal_automatic_value() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    root.style.height = Some(52.0);
    let automatic = PanelState::from_root(&root);
    assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 50);

    root.panel.exclusive_zone = Some(52);
    let explicit = PanelState::from_root(&root);
    assert_eq!(automatic.geometry, explicit.geometry);
    assert_ne!(automatic, explicit);
    assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 52);

    root.panel.exclusive_zone = None;
    let automatic_again = PanelState::from_root(&root);
    assert_ne!(explicit, automatic_again);
    assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 50);
}

#[test]
fn rejects_unknown_or_invalid_output_dimensions() {
    assert_eq!(valid_dimensions((0, 1080)), None);
    assert_eq!(valid_dimensions((-1, 1080)), None);
    assert_eq!(fallback_logical_size((1, 1), 2, false), None);
}

#[test]
fn preflights_keyboard_modes_against_layer_shell_version() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let surfaces = |root| {
        vec![SurfaceTree {
            key: SurfaceKey::Default,
            namespace: "thing".into(),
            root,
        }]
    };

    assert!(validate_surface_capabilities(&surfaces(root.clone()), 3, false).is_err());
    assert!(validate_surface_capabilities(&surfaces(root.clone()), 4, false).is_ok());
    root.panel.keyboard_mode = KeyboardMode::None;
    assert!(validate_surface_capabilities(&surfaces(root), 1, false).is_ok());
}

#[test]
fn reactive_capability_rejection_preserves_the_surface_and_can_recover() {
    let directory = tempdir().unwrap();
    let entry = directory.path().join("panel.luau");
    fs::write(
        &entry,
        r#"
            local demand = ui.state(false)
            return ui.panel {
                keyboard_mode = function()
                    return if demand:get() then 'on_demand' else 'none'
                end,
                children = { ui.button {
                    on_click = function() demand:set(not demand:get()) end,
                } },
            }
            "#,
    )
    .unwrap();
    let output = OutputContext {
        name: Some("DP-1".into()),
        width: 1920,
        height: 1080,
        scale: Scale::default(),
    };
    let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
    configurations.add(1, output).unwrap();
    let button = configurations.root(&1).unwrap().children[0].id;
    let validate = |surfaces: &[SurfaceTree]| validate_surface_capabilities(surfaces, 3, false);

    let error = configurations
        .dispatch_checked(&1, button, Event::Click, validate)
        .unwrap_err();
    assert!(error.contains("on_demand"), "{error}");
    assert_eq!(
        configurations.root(&1).unwrap().panel.keyboard_mode,
        KeyboardMode::None
    );

    let outcome = configurations
        .dispatch_checked(&1, button, Event::Click, validate)
        .unwrap();
    assert!(!outcome.changed);
    assert!(outcome.actions.is_empty());
    assert!(outcome.ui_actions.is_empty());
    assert_eq!(
        configurations.root(&1).unwrap().panel.keyboard_mode,
        KeyboardMode::None
    );
}

#[test]
fn popup_lookup_keeps_manual_and_automatic_activation_separate() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut button = Node::new(NodeId(2), Kind::Button);
    let mut automatic = Node::new(NodeId(3), Kind::Popup);
    automatic.popup.open_on_hover = true;
    button.children.push(automatic);
    button.children.push(Node::new(NodeId(4), Kind::Popup));
    root.children.push(button);

    assert_eq!(
        find_manual_popup(&root, None),
        Some((NodeId(4), Some(NodeId(2))))
    );
    assert_eq!(
        find_automatic_popup(&root, NodeId(2), None).map(|node| node.id),
        Some(NodeId(3))
    );
}

#[test]
fn interactive_path_keeps_ancestors_hovered_across_descendants() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut outer = Node::new(NodeId(2), Kind::Button);
    outer.children.push(Node::new(NodeId(3), Kind::Button));
    root.children.push(outer);

    assert_eq!(
        interactive_path(&root, NodeId(3)),
        Some(vec![NodeId(2), NodeId(3)])
    );
}
