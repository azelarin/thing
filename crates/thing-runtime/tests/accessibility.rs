use std::fs;
use thing_core::{AccessibilityChecked, AccessibilityLive, AccessibilityRole};
use thing_runtime::{CallValue, Event, Runtime};

fn load(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn rejected(source: &str, expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let error = Runtime::load(path)
        .err()
        .expect("expected invalid metadata")
        .to_string();
    assert!(error.contains(expected), "{error}");
}

#[test]
fn names_and_control_states_follow_their_normal_reactive_bindings() {
    let (_directory, mut runtime) = load(
        r#"
        local muted=ui.state(false)
        return ui.panel {accessibility={role='toolbar',label='media'},children={
            ui.button {
                accessibility={role='switch',description='microphone state',
                    label=function() return if muted:get() then 'unmute' else 'mute' end,
                    checked=function() return muted:get() end},
                on_click=function() muted:set(not muted:get()) end,
            },
            ui.text {text='connected',accessibility={role='status',live='polite'}},
        }}
    "#,
    );
    let root = runtime.root().unwrap();
    assert_eq!(root.accessibility.role, Some(AccessibilityRole::Toolbar));
    let button = &root.children[0];
    let id = button.id;
    assert_eq!(button.accessibility.label.as_deref(), Some("mute"));
    assert_eq!(
        button.accessibility.checked,
        Some(AccessibilityChecked::Unchecked)
    );
    assert_eq!(
        root.children[1].accessibility.live,
        Some(AccessibilityLive::Polite)
    );
    runtime.dispatch(id, Event::Click).unwrap();
    let button = &runtime.root().unwrap().children[0];
    assert_eq!(button.accessibility.label.as_deref(), Some("unmute"));
    assert_eq!(
        button.accessibility.checked,
        Some(AccessibilityChecked::Checked)
    );
}

#[test]
fn optional_metadata_can_be_cleared_and_failed_updates_keep_the_previous_tree() {
    let (_directory, mut runtime) = load(
        r#"
        local label=ui.state('good')
        ui.command('rename',function(value) label:set(value) end)
        return ui.panel {children={ui.button {accessibility={
            label=function() return label:get() end,
        }}}}
    "#,
    );
    let call = |runtime: &mut Runtime, value: CallValue| {
        runtime.call_command_checked("rename", &value, |_| Ok(()))
    };
    assert!(call(&mut runtime, CallValue::String("x".repeat(4097))).is_err());
    assert_eq!(
        runtime.root().unwrap().children[0]
            .accessibility
            .label
            .as_deref(),
        Some("good")
    );
    call(&mut runtime, CallValue::Nil).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].accessibility.label,
        None
    );
}

#[test]
fn metadata_tables_roles_and_announcement_modes_are_static() {
    for (source, expected) in [
        (
            "return ui.panel {accessibility=function() return {} end}",
            "static table",
        ),
        (
            "return ui.panel {accessibility={role=function() return 'group' end}}",
            "must be static",
        ),
        (
            "return ui.panel {accessibility={live=function() return 'polite' end}}",
            "must be static",
        ),
        (
            "return ui.panel {accessibility={role='not-a-role'}}",
            "unknown accessibility role",
        ),
        (
            "return ui.panel {accessibility={unknown=true}}",
            "unknown accessibility property",
        ),
    ] {
        rejected(source, expected);
    }
}

#[test]
fn static_metadata_cannot_be_rebound_through_a_dotted_property() {
    rejected(
        "return ui.panel {['accessibility.role']=function() return 'group' end}",
        "must be static",
    );
}

#[test]
fn roles_and_states_cannot_hide_or_misrepresent_interactive_controls() {
    for (source, expected) in [
        (
            "return ui.panel {accessibility={hidden=true}}",
            "decorative",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={hidden=true}}}}",
            "decorative",
        ),
        (
            "return ui.panel {children={ui.text {text='x',accessibility={role='switch'}}}}",
            "not supported",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={checked=true}}}}",
            "checked needs",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={role='switch',checked='mixed'}}}}",
            "mixed state",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={selected=true}}}}",
            "selected needs",
        ),
        (
            "return ui.panel {children={ui.text {text='x',accessibility={expanded=false}}}}",
            "expanded needs",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={live='polite'}}}}",
            "live needs",
        ),
        (
            "return ui.panel {children={ui.button {accessibility={value_text='42'}}}}",
            "value_text needs",
        ),
    ] {
        rejected(source, expected);
    }
    let (_directory, runtime) = load(
        "return ui.panel {children={ui.button {accessibility={role='checkbox',checked='mixed'}},ui.text {text='decoration',accessibility={hidden=true}}}} ",
    );
    assert_eq!(
        runtime.root().unwrap().children[0].accessibility.checked,
        Some(AccessibilityChecked::Mixed)
    );
    assert!(runtime.root().unwrap().children[1].accessibility.hidden);
}

#[test]
fn metadata_strings_are_bounded_without_truncation_or_nul() {
    for field in ["label", "description", "value_text"] {
        let widget = if field == "value_text" {
            "slider"
        } else {
            "button"
        };
        for value in ["string.rep('x',4097)", "'a'..string.char(0)..'b'"] {
            rejected(
                &format!(
                    "return ui.panel {{children={{ui.{widget} {{accessibility={{{field}={value}}}}}}}}}"
                ),
                "4096 bytes without NUL",
            );
        }
    }
}

#[test]
fn secret_inputs_accept_a_name_but_cannot_publish_a_value_description() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    fs::write(
        &path,
        "return ui.lock {children={ui.secret_input {accessibility={label='password'}}}}",
    )
    .unwrap();
    let runtime = Runtime::load_lock(&path).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0]
            .accessibility
            .label
            .as_deref(),
        Some("password")
    );
    fs::write(
        &path,
        "return ui.lock {children={ui.secret_input {accessibility={value_text='entered value'}}}}",
    )
    .unwrap();
    assert!(
        Runtime::load_lock(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("value_text")
    );
}
