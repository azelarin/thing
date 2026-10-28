use std::fs;

use thing_runtime::{Action, Event, Runtime};

fn runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

#[test]
fn launch_preserves_arguments_and_working_directory() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button {
            on_click = function()
                services.launch({ 'program', 'with space', "'quoted'", '$HOME', 'one;two', '' }, '/tmp/work dir')
            end,
        } } }
        "#,
    );

    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Launch {
            argv: vec![
                "program".into(),
                "with space".into(),
                "'quoted'".into(),
                "$HOME".into(),
                "one;two".into(),
                String::new(),
            ],
            cwd: Some("/tmp/work dir".into()),
        }]
    );
}

#[test]
fn launch_accepts_an_omitted_working_directory() {
    let (_directory, mut runtime) = runtime(
        "return ui.panel { children = { ui.button { on_click = function() services.launch({ 'program' }) end } } }",
    );

    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Launch {
            argv: vec!["program".into()],
            cwd: None,
        }]
    );
}

#[test]
fn launch_rejects_invalid_arguments() {
    for arguments in [
        "{}",
        "{ '' }",
        "{ 'program', 1 }",
        "{ 'program', label = 'extra' }",
        "{ [1] = 'program', [3] = 'gap' }",
        "{ 'program', string.char(0) }",
        "{ string.char(255) }",
        "{ 'program', string.rep('a', 4097) }",
        "(function() local value = {}; for i = 1, 257 do value[i] = 'a' end; return value end)()",
        "(function() local value = { 'program' }; for i = 2, 18 do value[i] = string.rep('a', 4096) end; return value end)()",
    ] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.button {{ on_click = function() services.launch({arguments}) end }} }} }}"
        );
        let (_directory, mut runtime) = runtime(&source);
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {arguments}"
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn launch_rejects_invalid_working_directories() {
    for cwd in [
        "''",
        "1",
        "string.char(0)",
        "string.char(255)",
        "string.rep('a', 4097)",
    ] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.button {{ on_click = function() services.launch({{ 'program' }}, {cwd}) end }} }} }}"
        );
        let (_directory, mut runtime) = runtime(&source);
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {cwd}"
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn failed_events_discard_launches() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = {
            ui.button { on_click = function() services.launch({ 'first' }) end },
            ui.button { on_click = function()
                services.launch({ 'discarded' })
                error('broken handler')
            end },
        } }
        "#,
    );
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert!(
        runtime
            .dispatch(runtime.root().unwrap().children[1].id, Event::Click)
            .is_err()
    );
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Launch {
            argv: vec!["first".into()],
            cwd: None,
        }]
    );
}

#[test]
fn launch_is_allowed_only_in_event_handlers() {
    for source in [
        "services.launch({ 'program' }); return ui.panel {}",
        "return ui.panel { visible = function() services.launch({ 'program' }); return true end }",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err());
    }
}

#[test]
fn launch_uses_the_service_action_limit() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            for _ = 1, 129 do services.launch({ 'program' }) end
        end } } }
        "#,
    );

    assert!(
        runtime
            .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
            .is_err()
    );
    assert!(runtime.take_actions().is_empty());
}
