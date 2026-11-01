use std::{collections::BTreeMap, num::NonZeroU32, time::Duration};

use smithay_client_toolkit::seat::{
    keyboard::{KeyEvent, Keysym, Modifiers, RepeatInfo},
    pointer::AxisScroll,
};
use thing_core::{Kind, Node, NodeId, Point, Rect, ScrollAxis};
use thing_ui::{Layout, Measurer, Size, TextInputEditor};
use wayland_client::protocol::wl_pointer;

use super::{
    ActiveTextInput, GenerationReset, MissingSurfacePolicy, PointerPhase, RepeatSession,
    RepeatTimerAction, RuntimePointerButton, RuntimeScrollSource, SliderKeyAction,
    SurfaceActionRoute, SurfaceLayouts, TextClipboardAction, TextKeyAction, WidgetKeyAction,
    active_text_input_is_current, allocate_text_input_generation,
    cancel_active_text_input_composition, claim_autofocus, direct_input_serial,
    focused_widget_is_interactive, input_method_cursor_rectangle,
    input_method_surface_has_keyboard_focus, keyboard_focus_transition_affects,
    keyboard_leave_matches, keyboard_surface_changed, paint_interaction, pending_paste_is_current,
    prepare_text_input_submit, reconcile_controlled_text_edit, repeat_action_is_current,
    reset_active_text_input_generation, retain_non_focus_actions, reveal_text_input_focus,
    root_key_input, runtime_pointer_event, runtime_scroll_input, scroll_distance, scroll_steps,
    scroll_view_axis_matches, semantic_pointer_target, step_link, surface_action_route,
    text_clipboard_action, text_key_action, viewport_scroll_delta, widget_key_action,
};

struct FixedMeasurer;

impl Measurer for FixedMeasurer {
    fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
        Size {
            width: 20.0,
            height: 20.0,
        }
    }
}

#[test]
fn text_input_generations_never_wrap_or_repeat() {
    let mut next = u64::MAX;
    assert_eq!(allocate_text_input_generation(&mut next), Some(u64::MAX));
    assert_eq!(next, 0);
    assert_eq!(allocate_text_input_generation(&mut next), None);
    assert_eq!(next, 0);
}

#[test]
fn custom_pointer_coordinates_are_local_to_current_presented_bounds() {
    let modifiers = Modifiers {
        shift: true,
        logo: true,
        ..Modifiers::default()
    };
    let event = runtime_pointer_event(
        Point { x: 130.5, y: -15.0 },
        PointerPhase::Move,
        0x110,
        (170.5, 10.0),
        &modifiers,
    )
    .unwrap();
    assert_eq!(event.phase, PointerPhase::Move);
    assert_eq!(event.button, RuntimePointerButton::Primary);
    assert_eq!((event.x, event.y), (130.5, -15.0));
    assert_eq!((event.surface_x, event.surface_y), (170.5, 10.0));
    assert!(event.shift);
    assert!(event.logo);
    assert!(!event.ctrl);
    assert!(!event.alt);
    assert!(
        runtime_pointer_event(
            Point::default(),
            PointerPhase::Press,
            0x113,
            (0.0, 0.0),
            &Modifiers::default(),
        )
        .is_none()
    );
}

#[test]
fn custom_pointer_children_block_parent_button_defaults() {
    let button = NodeId(1);
    let area = NodeId(2);
    assert_eq!(semantic_pointer_target(Some(button), Some(area)), None);
    assert_eq!(
        semantic_pointer_target(Some(button), Some(button)),
        Some(button),
    );
    assert_eq!(semantic_pointer_target(Some(button), None), Some(button));
    assert_eq!(semantic_pointer_target(None, Some(area)), None);
}

#[test]
fn autofocus_is_claimed_once_and_never_overrides_a_choice() {
    let mut available = true;
    assert!(claim_autofocus(&mut available, None, false));
    assert!(!claim_autofocus(&mut available, None, false));

    let mut available = true;
    assert!(!claim_autofocus(&mut available, Some(NodeId(1)), false));
    assert!(!available);

    let mut available = true;
    assert!(!claim_autofocus(&mut available, None, true));
    assert!(!available);
}

#[test]
fn popup_actions_wait_for_same_cycle_creation_and_then_expire() {
    assert_eq!(
        surface_action_route(None, MissingSurfacePolicy::WaitForSameCycle),
        SurfaceActionRoute::Retain
    );
    assert_eq!(
        surface_action_route(Some(4), MissingSurfacePolicy::WaitForSameCycle),
        SurfaceActionRoute::Assign(4)
    );
    assert_eq!(
        surface_action_route(None, MissingSurfacePolicy::Expire),
        SurfaceActionRoute::Drop
    );
}

#[test]
fn keyboard_loss_cancels_focus_requests_but_keeps_reveals() {
    let mut actions = vec![("focus", true), ("reveal", false), ("focus", true)];
    retain_non_focus_actions(&mut actions, |(_, focus)| *focus);
    assert_eq!(actions, [("reveal", false)]);
}

#[test]
fn painting_hides_focus_without_changing_its_input_origin() {
    let mut keyboard = thing_ui::Interaction::default();
    keyboard.focus_from_keyboard(NodeId(1));
    assert_eq!(paint_interaction(keyboard, false).keyboard_focused(), None);
    assert_eq!(keyboard.keyboard_focused(), Some(NodeId(1)));
    assert_eq!(
        paint_interaction(keyboard, true).keyboard_focused(),
        Some(NodeId(1))
    );

    let mut pointer = thing_ui::Interaction::default();
    pointer.focus_from_pointer(Some(NodeId(2)));
    assert_eq!(paint_interaction(pointer, true).keyboard_focused(), None);
}

#[test]
fn input_layouts_do_not_replace_pointer_geometry_before_presentation() {
    let mut layouts = SurfaceLayouts::default();
    layouts.publish(
        slider_layout(false, true),
        NodeId(1),
        "consumer".into(),
        BTreeMap::new(),
    );
    assert!(
        !layouts
            .presented()
            .unwrap()
            .focus_order()
            .any(|id| id == NodeId(3))
    );

    layouts.stage_input(slider_layout(true, true));
    assert_eq!(
        layouts.presented_consumer(),
        Some("consumer"),
        "staging input geometry must preserve presented widget identities",
    );
    assert!(
        !layouts
            .presented()
            .unwrap()
            .focus_order()
            .any(|id| id == NodeId(3))
    );
    assert!(
        layouts
            .input()
            .unwrap()
            .focus_order()
            .any(|id| id == NodeId(3))
    );

    layouts.invalidate_input();
    assert!(layouts.input_needs_refresh());
    assert!(
        !layouts
            .input()
            .unwrap()
            .focus_order()
            .any(|id| id == NodeId(3))
    );

    layouts.stage_input(slider_layout(true, true));
    assert!(!layouts.input_needs_refresh());
    assert!(
        layouts
            .input()
            .unwrap()
            .focus_order()
            .any(|id| id == NodeId(3))
    );
}

#[test]
fn a_focus_change_during_dispatch_cannot_rearm_key_repeat() {
    assert!(repeat_action_is_current(true, &Some((1, 2)), &Some((1, 2))));
    assert!(!repeat_action_is_current(
        true,
        &Some((1, 2)),
        &Some((1, 3))
    ));
    assert!(!repeat_action_is_current(true, &Some((1, 2)), &None));
    assert!(!repeat_action_is_current(
        false,
        &Some((1, 2)),
        &Some((1, 2))
    ));
}

#[test]
fn input_method_cursor_rectangle_is_surface_local_and_nonempty() {
    let mut node = Node::new(NodeId(1), Kind::TextInput);
    node.text = "hello".into();
    node.style.padding = 4.0;
    let editor = TextInputEditor::new(&node, 112.0);
    let rectangle = input_method_cursor_rectangle(
        &node,
        Rect {
            width: 120.0,
            height: 32.0,
            ..Rect::default()
        },
        thing_ui::Affine::translation(20.0, 10.0),
        true,
        &editor,
    )
    .unwrap();

    assert!(rectangle.x >= 24);
    assert!(rectangle.y >= 14);
    assert!(rectangle.width >= 1);
    assert!(rectangle.height >= 1);
}

#[test]
fn input_method_cursor_rectangle_tracks_scrolled_rtl_preedit() {
    let mut node = Node::new(NodeId(1), Kind::TextInput);
    node.text = "abc אבג ".repeat(20);
    node.style.padding = 4.0;
    let mut editor = TextInputEditor::new(&node, 32.0);
    editor
        .apply_ime_batch(thing_ui::TextInputImeBatch {
            preedit: Some(thing_ui::TextInputPreedit {
                text: "候補".into(),
                cursor: Some((6, 0)),
            }),
            ..thing_ui::TextInputImeBatch::default()
        })
        .unwrap();
    let rectangle = input_method_cursor_rectangle(
        &node,
        Rect {
            width: 40.0,
            height: 32.0,
            ..Rect::default()
        },
        thing_ui::Affine::translation(50.0, 12.0),
        true,
        &editor,
    )
    .unwrap();

    assert!((54..=86).contains(&rectangle.x));
    assert!(rectangle.y >= 16);
    assert!(rectangle.width >= 1);
    assert!(rectangle.height >= 1);
}

#[test]
fn input_method_cursor_geometry_rotates_outward_and_singular_geometry_is_unavailable() {
    let mut node = Node::new(NodeId(1), Kind::TextInput);
    node.text = "hello".into();
    node.style.padding = 4.0;
    let editor = TextInputEditor::new(&node, 112.0);
    let mut style = thing_core::Style {
        rotation: 90.0,
        translate_x: 40.0,
        ..thing_core::Style::default()
    };
    style.transform_origin.x = 0.0;
    style.transform_origin.y = 0.0;
    let transform = thing_ui::Affine::widget(&style, 120.0, 32.0).unwrap();
    let rectangle = input_method_cursor_rectangle(
        &node,
        Rect {
            width: 120.0,
            height: 32.0,
            ..Rect::default()
        },
        transform,
        true,
        &editor,
    )
    .unwrap();
    assert!(rectangle.x <= 36);
    assert!(rectangle.width >= 1);
    assert!(rectangle.height >= 1);

    assert!(
        input_method_cursor_rectangle(
            &node,
            Rect {
                width: 120.0,
                height: 32.0,
                ..Rect::default()
            },
            transform,
            false,
            &editor,
        )
        .is_none()
    );
}

#[test]
fn refreshed_tree_invalidates_stale_text_input_targets() {
    assert!(input_method_surface_has_keyboard_focus(&1, Some(&1)));
    assert!(!input_method_surface_has_keyboard_focus(&1, Some(&2)));
    assert!(!input_method_surface_has_keyboard_focus::<u8>(&1, None));

    let mut root = Node::new(NodeId(1), Kind::Panel);
    root.style.width = Some(200.0);
    root.style.height = Some(50.0);
    let mut row = Node::new(NodeId(2), Kind::Row);
    row.children.push(Node::new(NodeId(3), Kind::TextInput));
    root.children.push(row);
    let layout = Layout::compute(
        &root,
        Size {
            width: 200.0,
            height: 50.0,
        },
        &mut FixedMeasurer,
    )
    .unwrap();
    assert!(active_text_input_is_current(
        Some(&root),
        Some(&layout),
        Some(NodeId(3)),
        NodeId(3)
    ));

    let mut singular = root.clone();
    singular.children[0].style.scale_x = 0.0;
    let singular_layout = Layout::compute(
        &singular,
        Size {
            width: 200.0,
            height: 50.0,
        },
        &mut FixedMeasurer,
    )
    .unwrap();
    assert!(singular_layout.get(NodeId(3)).is_none());
    let semantic = *singular_layout.semantic_placement(NodeId(3)).unwrap();
    assert!(active_text_input_is_current(
        Some(&singular),
        Some(&singular_layout),
        Some(NodeId(3)),
        NodeId(3)
    ));
    let node = singular.find(NodeId(3)).unwrap();
    let editor = TextInputEditor::new(node, semantic.local_bounds().width);
    assert!(
        input_method_cursor_rectangle(
            node,
            semantic.local_bounds(),
            semantic.transform,
            semantic.is_invertible(),
            &editor,
        )
        .is_none()
    );

    let mut hidden = root.clone();
    hidden.children[0].visible = false;
    assert!(!active_text_input_is_current(
        Some(&hidden),
        Some(&layout),
        Some(NodeId(3)),
        NodeId(3)
    ));
    let mut disabled = root.clone();
    disabled.children[0].enabled = false;
    assert!(!active_text_input_is_current(
        Some(&disabled),
        Some(&layout),
        Some(NodeId(3)),
        NodeId(3)
    ));
    let mut transformed = root.clone();
    transformed.children[0].children[0].kind = Kind::Text;
    assert!(!active_text_input_is_current(
        Some(&transformed),
        Some(&layout),
        Some(NodeId(3)),
        NodeId(3)
    ));
    root.children[0].children.clear();
    assert!(!active_text_input_is_current(
        Some(&root),
        Some(&layout),
        Some(NodeId(3)),
        NodeId(3)
    ));
}

fn slider_layout(parent_enabled: bool, slider_visible: bool) -> Layout {
    let mut panel = Node::new(NodeId(1), Kind::Panel);
    panel.style.width = Some(200.0);
    panel.style.height = Some(50.0);
    let mut row = Node::new(NodeId(2), Kind::Row);
    row.enabled = parent_enabled;
    let mut slider = Node::new(NodeId(3), Kind::Slider);
    slider.visible = slider_visible;
    slider.style.width = Some(100.0);
    slider.style.height = Some(30.0);
    row.children.push(slider);
    panel.children.push(row);
    Layout::compute(
        &panel,
        Size {
            width: 200.0,
            height: 50.0,
        },
        &mut FixedMeasurer,
    )
    .unwrap()
}

fn key_event(raw_code: u32, keysym: Keysym) -> KeyEvent {
    KeyEvent {
        time: 10,
        raw_code,
        keysym,
        utf8: None,
    }
}

fn right_key(raw_code: u32) -> KeyEvent {
    key_event(raw_code, Keysym::Right)
}

fn enabled_repeat() -> RepeatInfo {
    RepeatInfo::Repeat {
        rate: NonZeroU32::new(25).unwrap(),
        delay: 600,
    }
}

#[test]
fn viewport_wheel_deltas_preserve_axes_and_shift_only_vertical_input() {
    assert_eq!(
        viewport_scroll_delta(0.25, 0.5, false),
        Point { x: 10.0, y: 20.0 }
    );
    assert_eq!(
        viewport_scroll_delta(0.0, -0.5, true),
        Point { x: -20.0, y: 0.0 }
    );
    assert_eq!(
        viewport_scroll_delta(0.25, 0.5, true),
        Point { x: 10.0, y: 20.0 }
    );
    assert_eq!(
        viewport_scroll_delta(f64::MAX, 0.5, false),
        Point { x: 0.0, y: 20.0 }
    );
}

#[test]
fn viewport_axis_changes_cannot_reuse_a_captured_wheel_route() {
    struct Fixed;
    impl Measurer for Fixed {
        fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
            Size::default()
        }
    }
    let mut node = Node::new(NodeId(1), Kind::Scroll);
    let size = Size {
        width: 100.0,
        height: 40.0,
    };
    let layout = Layout::compute(&node, size, &mut Fixed).unwrap();
    assert!(scroll_view_axis_matches(
        &layout,
        &node,
        ScrollAxis::Vertical
    ));
    node.scroll_axis = ScrollAxis::Horizontal;
    assert!(!scroll_view_axis_matches(
        &layout,
        &node,
        ScrollAxis::Vertical
    ));
    assert!(!scroll_view_axis_matches(
        &layout,
        &node,
        ScrollAxis::Horizontal
    ));
    let layout = Layout::compute(&node, size, &mut Fixed).unwrap();
    assert!(scroll_view_axis_matches(
        &layout,
        &node,
        ScrollAxis::Horizontal
    ));
}

#[test]
fn scroll_steps_convert_to_finite_logical_distance() {
    assert_eq!(
        scroll_steps(AxisScroll {
            value120: 30,
            ..AxisScroll::default()
        }),
        0.25
    );
    assert_eq!(
        scroll_steps(AxisScroll {
            discrete: -1,
            ..AxisScroll::default()
        }),
        -1.0
    );
    assert_eq!(
        scroll_steps(AxisScroll {
            absolute: 7.5,
            ..AxisScroll::default()
        }),
        0.5
    );
    assert_eq!(scroll_distance(1.0), Some(40.0));
    assert_eq!(scroll_distance(-0.5), Some(-20.0));
    assert_eq!(scroll_distance(0.0), None);
    assert_eq!(scroll_distance(f64::NAN), None);
    assert_eq!(scroll_distance(f64::INFINITY), None);
    assert_eq!(scroll_distance(f64::MAX), None);
}

#[test]
fn scroll_metadata_uses_handler_local_coordinates_and_axis_source() {
    let modifiers = Modifiers {
        shift: true,
        ctrl: false,
        alt: true,
        logo: false,
        ..Modifiers::default()
    };
    let input = runtime_scroll_input(
        Point { x: 4.0, y: 5.0 },
        0.25,
        -0.5,
        (14.0, 25.0),
        &modifiers,
        Some(wl_pointer::AxisSource::WheelTilt),
    )
    .unwrap();
    assert_eq!(input.x, 0.25);
    assert_eq!(input.y, -0.5);
    assert_eq!(input.local_x, 4.0);
    assert_eq!(input.local_y, 5.0);
    assert_eq!(input.surface_x, 14.0);
    assert_eq!(input.surface_y, 25.0);
    assert!(input.shift);
    assert!(!input.ctrl);
    assert!(input.alt);
    assert!(!input.logo);
    assert_eq!(input.source, RuntimeScrollSource::WheelTilt);

    assert!(
        runtime_scroll_input(
            Point::default(),
            f64::NAN,
            0.0,
            (0.0, 0.0),
            &Modifiers::default(),
            None,
        )
        .is_none()
    );
}

#[test]
fn button_activation_never_repeats() {
    assert_eq!(
        widget_key_action(Kind::Button, Keysym::Return, true),
        Some(WidgetKeyAction::Activate)
    );
    assert_eq!(
        widget_key_action(Kind::Button, Keysym::space, true),
        Some(WidgetKeyAction::Activate)
    );
    assert_eq!(widget_key_action(Kind::Button, Keysym::Return, false), None);
    assert_eq!(widget_key_action(Kind::Button, Keysym::space, false), None);
    assert!(!WidgetKeyAction::Activate.repeats());
}

#[test]
fn root_key_records_use_xkb_names_text_modifiers_and_repeat_origin() {
    let mut event = key_event(108, Keysym::Down);
    event.utf8 = Some("é".into());
    let modifiers = Modifiers {
        shift: true,
        ctrl: true,
        alt: false,
        logo: true,
        ..Modifiers::default()
    };

    let input = root_key_input(&event, modifiers, true);

    assert_eq!(input.key, "Down");
    assert_eq!(input.text.as_deref(), Some("é"));
    assert!(input.shift);
    assert!(input.ctrl);
    assert!(!input.alt);
    assert!(input.logo);
    assert!(input.repeated);

    event.utf8 = Some(String::new());
    assert_eq!(root_key_input(&event, modifiers, false).text, None);

    event.keysym = Keysym::new(0xdead_beef);
    assert_eq!(root_key_input(&event, modifiers, false).key, "0xdeadbeef");
}

#[test]
fn slider_arrows_repeat_but_endpoints_do_not() {
    assert_eq!(
        widget_key_action(Kind::Slider, Keysym::Left, false),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
    );
    assert_eq!(
        widget_key_action(Kind::Slider, Keysym::Right, false),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Increase))
    );
    assert_eq!(
        widget_key_action(Kind::Slider, Keysym::Home, true),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
    );
    assert_eq!(
        widget_key_action(Kind::Slider, Keysym::End, true),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Maximum))
    );
    assert_eq!(widget_key_action(Kind::Slider, Keysym::Home, false), None);
    assert_eq!(widget_key_action(Kind::Slider, Keysym::End, false), None);
    assert!(WidgetKeyAction::Slider(SliderKeyAction::Decrease).repeats());
    assert!(WidgetKeyAction::Slider(SliderKeyAction::Increase).repeats());
    assert!(!WidgetKeyAction::Slider(SliderKeyAction::Minimum).repeats());
    assert!(!WidgetKeyAction::Slider(SliderKeyAction::Maximum).repeats());

    assert_eq!(
        widget_key_action(Kind::Range, Keysym::Left, false),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
    );
    assert_eq!(
        widget_key_action(Kind::Range, Keysym::Home, true),
        Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
    );
}

#[test]
fn only_direct_input_can_supply_an_activation_serial() {
    assert_eq!(direct_input_serial(true, 42), Some(42));
    assert_eq!(direct_input_serial(false, 42), None);
}

#[test]
fn text_actions_separate_editing_shortcuts_from_direct_text() {
    let mut event = key_event(30, Keysym::e);
    event.utf8 = Some("é".into());
    let insert = text_key_action(&event, Modifiers::default(), true).unwrap();
    assert_eq!(insert, TextKeyAction::Insert("é".into()));
    assert!(insert.repeats());
    assert!(insert.changes_value());

    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::default()
    };
    let select = text_key_action(&key_event(30, Keysym::a), ctrl, true).unwrap();
    assert_eq!(select, TextKeyAction::SelectAll);
    assert!(!select.repeats());
    assert!(!select.changes_value());
    assert_eq!(text_key_action(&event, ctrl, true), None);

    let alt_gr = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::default()
    };
    assert_eq!(
        text_key_action(&event, alt_gr, true),
        Some(TextKeyAction::Insert("é".into()))
    );
    assert_eq!(
        text_key_action(&key_event(28, Keysym::Return), Modifiers::default(), true),
        Some(TextKeyAction::Submit)
    );
    assert_eq!(
        text_key_action(&key_event(28, Keysym::Return), Modifiers::default(), false),
        None
    );
    for action in [
        TextKeyAction::MoveLeft,
        TextKeyAction::MoveRight,
        TextKeyAction::MoveToStart,
        TextKeyAction::MoveToEnd,
    ] {
        assert!(!action.changes_value());
    }

    let mut separator = key_event(30, Keysym::e);
    separator.utf8 = Some("\u{2028}".into());
    assert_eq!(
        text_key_action(&separator, Modifiers::default(), true),
        None
    );
}

#[test]
fn clipboard_shortcuts_are_direct_ctrl_actions_without_alt_or_logo() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::default()
    };
    for (keysym, expected) in [
        (Keysym::c, TextClipboardAction::Copy),
        (Keysym::C, TextClipboardAction::Copy),
        (Keysym::x, TextClipboardAction::Cut),
        (Keysym::X, TextClipboardAction::Cut),
        (Keysym::v, TextClipboardAction::Paste),
        (Keysym::V, TextClipboardAction::Paste),
    ] {
        let event = key_event(30, keysym);
        assert_eq!(text_clipboard_action(&event, ctrl, true), Some(expected));
        assert_eq!(text_clipboard_action(&event, ctrl, false), None);
    }

    let event = key_event(30, Keysym::c);
    assert_eq!(
        text_clipboard_action(
            &event,
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            },
            true,
        ),
        Some(TextClipboardAction::Copy)
    );
    assert_eq!(
        text_clipboard_action(&event, Modifiers::default(), true),
        None
    );
    for modifiers in [
        Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::default()
        },
        Modifiers {
            ctrl: true,
            logo: true,
            ..Modifiers::default()
        },
    ] {
        assert_eq!(text_clipboard_action(&event, modifiers, true), None);
    }
    assert_eq!(
        text_clipboard_action(&key_event(30, Keysym::a), ctrl, true),
        None
    );
}

#[test]
fn paste_requires_the_original_surface_target_focus_generation_and_revision() {
    let id = NodeId(7);
    let expected = (id, 11, 13);
    assert!(pending_paste_is_current(
        true,
        true,
        Some(id),
        expected,
        Some(expected),
    ));
    assert!(!pending_paste_is_current(
        false,
        true,
        Some(id),
        expected,
        Some(expected),
    ));
    assert!(!pending_paste_is_current(
        true,
        false,
        Some(id),
        expected,
        Some(expected),
    ));
    assert!(!pending_paste_is_current(
        true,
        true,
        Some(NodeId(8)),
        expected,
        Some(expected),
    ));
    assert!(!pending_paste_is_current(
        true,
        true,
        Some(id),
        expected,
        Some((id, 12, 13)),
    ));
    assert!(!pending_paste_is_current(
        true,
        true,
        Some(id),
        expected,
        Some((id, 11, 14)),
    ));
}

#[test]
fn controlled_edits_keep_only_the_runtime_value() {
    let mut node = Node::new(NodeId(1), Kind::TextInput);
    node.text = "old".into();
    let mut editor = TextInputEditor::new(&node, 100.0);
    let checkpoint = editor.checkpoint();
    editor.select_all();
    assert!(editor.replace_selection("proposed"));
    assert!(reconcile_controlled_text_edit(
        &mut editor,
        &node,
        100.0,
        "proposed",
        checkpoint
    ));
    assert_eq!(editor.value(), "old");

    let checkpoint = editor.checkpoint();
    editor.select_all();
    assert!(editor.replace_selection("accepted"));
    node.text = "accepted".into();
    assert!(!reconcile_controlled_text_edit(
        &mut editor,
        &node,
        100.0,
        "accepted",
        checkpoint
    ));
    assert_eq!(editor.value(), "accepted");
}

#[test]
fn rejected_controlled_edits_do_not_restore_old_composition() {
    let mut node = Node::new(NodeId(1), Kind::TextInput);
    node.text = "old".into();
    let mut editor = TextInputEditor::new(&node, 100.0);
    editor
        .apply_ime_batch(thing_ui::TextInputImeBatch {
            preedit: Some(thing_ui::TextInputPreedit {
                text: "x".into(),
                cursor: Some((1, 1)),
            }),
            ..thing_ui::TextInputImeBatch::default()
        })
        .unwrap();
    let checkpoint = editor.checkpoint();
    assert!(editor.replace_selection("proposed"));

    assert!(reconcile_controlled_text_edit(
        &mut editor,
        &node,
        100.0,
        "proposed",
        checkpoint
    ));

    assert_eq!(editor.value(), "old");
    assert!(!editor.is_composing());
}

#[test]
fn accepted_controlled_result_keeps_generation_but_rejected_and_invalid_reset() {
    let id = NodeId(1);
    let mut node = Node::new(id, Kind::TextInput);
    node.text = "old".into();
    let mut active = Some(ActiveTextInput {
        id,
        generation: 1,
        editor: TextInputEditor::new(&node, 100.0),
        cursor_available: true,
    });
    let mut next = 2;

    let editor = &mut active.as_mut().unwrap().editor;
    let checkpoint = editor.checkpoint();
    editor.select_all();
    editor.replace_selection("accepted");
    node.text = "accepted".into();
    let rejected = reconcile_controlled_text_edit(editor, &node, 100.0, "accepted", checkpoint);
    assert!(!rejected);
    assert_eq!(active.as_ref().unwrap().generation, 1);

    let editor = &mut active.as_mut().unwrap().editor;
    let checkpoint = editor.checkpoint();
    editor.select_all();
    editor.replace_selection("rejected");
    assert!(reconcile_controlled_text_edit(
        editor, &node, 100.0, "rejected", checkpoint,
    ));
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, 1, &mut next),
        GenerationReset::Reset
    );
    assert_eq!(active.as_ref().unwrap().generation, 2);

    let editor = &mut active.as_mut().unwrap().editor;
    let checkpoint = editor.checkpoint();
    editor.select_all();
    editor.replace_selection("proposal");
    node.text = "transformed".into();
    assert!(reconcile_controlled_text_edit(
        editor, &node, 100.0, "proposal", checkpoint,
    ));
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, 2, &mut next),
        GenerationReset::Reset
    );
    assert_eq!(active.as_ref().unwrap().generation, 3);

    let invalid_batch_generation = 3;
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, invalid_batch_generation, &mut next,),
        GenerationReset::Reset
    );
    assert_eq!(active.as_ref().unwrap().generation, 4);
    let unchanged_next = next;
    let stale_done_generation = invalid_batch_generation;
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, stale_done_generation, &mut next,),
        GenerationReset::Stale
    );
    assert_eq!(active.as_ref().unwrap().generation, 4);
    assert_eq!(next, unchanged_next);
    next = 0;
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, 4, &mut next),
        GenerationReset::Exhausted
    );
    assert!(active.is_none());
}

#[test]
fn submit_and_repeated_enter_clear_preedit_without_submitting_it() {
    let id = NodeId(1);
    let mut node = Node::new(id, Kind::TextInput);
    node.text = "old".into();
    let mut active = Some(ActiveTextInput {
        id,
        generation: 1,
        editor: TextInputEditor::new(&node, 100.0),
        cursor_available: true,
    });
    active
        .as_mut()
        .unwrap()
        .editor
        .apply_ime_batch(thing_ui::TextInputImeBatch {
            preedit: Some(thing_ui::TextInputPreedit {
                text: "compose".into(),
                cursor: Some((7, 7)),
            }),
            ..thing_ui::TextInputImeBatch::default()
        })
        .unwrap();

    let (value, cancelled) = prepare_text_input_submit(active.as_mut().unwrap());
    assert!(cancelled);
    assert_eq!(value, "old");
    assert!(!active.as_ref().unwrap().editor.is_composing());
    let mut next = 2;
    assert_eq!(
        reset_active_text_input_generation(&mut active, id, 1, &mut next),
        GenerationReset::Reset
    );

    active
        .as_mut()
        .unwrap()
        .editor
        .apply_ime_batch(thing_ui::TextInputImeBatch {
            preedit: Some(thing_ui::TextInputPreedit {
                text: "again".into(),
                cursor: None,
            }),
            ..thing_ui::TextInputImeBatch::default()
        })
        .unwrap();
    assert!(cancel_active_text_input_composition(&mut active));
    assert!(!cancel_active_text_input_composition(&mut active));
    assert_eq!(active.as_ref().unwrap().generation, 2);
}

#[test]
fn every_text_key_action_reveals_keyboard_focus() {
    let mut input = thing_ui::Interaction::default();
    input.focus_from_pointer(Some(NodeId(1)));
    assert_eq!(input.keyboard_focused(), None);
    let mut dirty = false;

    reveal_text_input_focus(&mut input, &mut dirty);

    assert_eq!(input.keyboard_focused(), Some(NodeId(1)));
    assert!(dirty);
}

#[test]
fn keyboard_focus_transitions_dirty_only_old_and_new_surfaces() {
    assert!(keyboard_focus_transition_affects(Some(&1), Some(&2), &1));
    assert!(keyboard_focus_transition_affects(Some(&1), Some(&2), &2));
    assert!(!keyboard_focus_transition_affects(Some(&1), Some(&2), &3));

    assert!(keyboard_focus_transition_affects(Some(&1), Some(&1), &1));
    assert!(keyboard_focus_transition_affects(Some(&1), None, &1));
    assert!(!keyboard_focus_transition_affects(Some(&1), None, &2));

    assert!(keyboard_leave_matches(Some(&1), &1));
    assert!(!keyboard_leave_matches(Some(&1), &2));
    assert!(!keyboard_leave_matches::<u8>(None, &1));
}

#[test]
fn keyboard_focus_leave_and_reentry_are_two_paste_cancelling_transitions() {
    assert!(keyboard_surface_changed(Some(&1), None));
    assert!(keyboard_surface_changed(None, Some(&1)));
    assert!(keyboard_surface_changed(Some(&1), Some(&2)));
    assert!(!keyboard_surface_changed(Some(&1), Some(&1)));
    assert!(!keyboard_surface_changed::<u8>(None, None));
}

#[test]
fn keyboard_actions_require_an_interactive_focused_placement() {
    let enabled = slider_layout(true, true);
    assert!(focused_widget_is_interactive(Some(&enabled), NodeId(3)));

    let disabled = slider_layout(false, true);
    assert!(!focused_widget_is_interactive(Some(&disabled), NodeId(3)));

    let hidden = slider_layout(true, false);
    assert!(!focused_widget_is_interactive(Some(&hidden), NodeId(3)));
    assert!(!focused_widget_is_interactive(None, NodeId(3)));
}

#[test]
fn repeat_session_transfers_ownership_without_losing_the_held_key() {
    let mut session = RepeatSession::<u8>::default();
    assert_eq!(session.update(RepeatInfo::Disable), RepeatTimerAction::None);
    assert_eq!(
        session.press(right_key(106), 7, true, true),
        RepeatTimerAction::None
    );
    assert!(session.accepts_compositor_repeat(106, &7));
    assert_eq!(session.key.as_ref().unwrap().event.raw_code, 106);

    assert_eq!(
        session.update(enabled_repeat()),
        RepeatTimerAction::Arm(Duration::from_millis(600))
    );
    assert!(session.host_is_armed());
    assert_eq!(session.key.as_ref().unwrap().surface, 7);
    assert!(!session.accepts_compositor_repeat(106, &7));

    assert_eq!(
        session.update(RepeatInfo::Disable),
        RepeatTimerAction::Disarm
    );
    assert!(!session.host_is_armed());
    assert_eq!(session.key.as_ref().unwrap().event.raw_code, 106);
    assert!(session.accepts_compositor_repeat(106, &7));
}

#[test]
fn release_and_focus_loss_clear_the_repeat_session() {
    let mut session = RepeatSession::<u8>::default();
    assert_eq!(session.update(enabled_repeat()), RepeatTimerAction::None);
    assert_eq!(
        session.press(right_key(106), 7, true, true),
        RepeatTimerAction::Arm(Duration::from_millis(600))
    );
    assert_eq!(session.release(105), RepeatTimerAction::None);
    assert!(session.key.is_some());
    assert_eq!(session.release(106), RepeatTimerAction::Disarm);
    assert!(session.key.is_none());
    assert!(!session.host_is_armed());

    assert_eq!(
        session.press(right_key(106), 8, true, true),
        RepeatTimerAction::Arm(Duration::from_millis(600))
    );
    assert_eq!(session.clear(), RepeatTimerAction::Disarm);
    assert!(session.key.is_none());
    assert!(!session.host_is_armed());
}

#[test]
fn physical_repeatability_controls_session_replacement() {
    let mut session = RepeatSession::<u8>::default();
    assert_eq!(session.update(enabled_repeat()), RepeatTimerAction::None);
    assert_eq!(
        session.press(right_key(106), 7, true, true),
        RepeatTimerAction::Arm(Duration::from_millis(600))
    );

    assert_eq!(
        session.press(key_event(42, Keysym::Shift_L), 8, false, false),
        RepeatTimerAction::None
    );
    let held = session.key.as_ref().unwrap();
    assert_eq!(held.event.raw_code, 106);
    assert_eq!(held.surface, 7);
    assert!(session.host_is_armed());

    assert_eq!(
        session.press(key_event(102, Keysym::Home), 8, true, false),
        RepeatTimerAction::Disarm
    );
    assert!(session.key.is_none());
    assert!(!session.host_is_armed());
}

#[test]
fn linked_text_keys_select_and_activate_links() {
    assert_eq!(
        widget_key_action(Kind::Text, Keysym::Return, true),
        Some(WidgetKeyAction::Activate)
    );
    assert_eq!(widget_key_action(Kind::Text, Keysym::Return, false), None);
    assert_eq!(
        widget_key_action(Kind::Text, Keysym::Right, false),
        Some(WidgetKeyAction::Link(1))
    );
    assert_eq!(
        widget_key_action(Kind::Text, Keysym::Up, false),
        Some(WidgetKeyAction::Link(-1))
    );
    assert!(WidgetKeyAction::Link(1).repeats());
    // stepping stops at either end; a stale selection restarts at an end.
    let links = [2, 5, 7];
    assert_eq!(step_link(&links, None, 1), Some(2));
    assert_eq!(step_link(&links, None, -1), Some(7));
    assert_eq!(step_link(&links, Some(5), 1), Some(7));
    assert_eq!(step_link(&links, Some(7), 1), Some(7));
    assert_eq!(step_link(&links, Some(2), -1), Some(2));
    assert_eq!(step_link(&links, Some(9), 1), Some(2));
    assert_eq!(step_link(&[], None, 1), None);
}
