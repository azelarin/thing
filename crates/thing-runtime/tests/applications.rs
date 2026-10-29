use std::fs;

use thing_runtime::{
    Action, ApplicationsSnapshot, DesktopAction, DesktopApplication, Event, Runtime,
};

fn runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn application(id: &str, name: &str) -> DesktopApplication {
    DesktopApplication {
        id: id.into(),
        name: name.into(),
        generic_name: "Web Browser".into(),
        comment: "Browse the web".into(),
        icon: "firefox".into(),
        keywords: vec!["internet".into(), "web".into()],
        categories: vec!["Network".into()],
        command: vec!["/usr/lib/firefox/firefox".into()],
        working_directory: Some("/tmp".into()),
        terminal: false,
        no_display: false,
        startup_wm_class: "firefox".into(),
        dbus_activatable: false,
        actions: vec![DesktopAction {
            key: "new-private-window".into(),
            name: "New Private Window".into(),
            icon: String::new(),
            command: vec!["/usr/lib/firefox/firefox".into(), "--private-window".into()],
        }],
    }
}

fn snapshot(applications: Vec<DesktopApplication>) -> ApplicationsSnapshot {
    ApplicationsSnapshot {
        available: true,
        error: String::new(),
        applications,
    }
}

#[test]
fn application_entries_are_reactive_and_read_only() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local apps = services.applications
                local first = apps.entries:get()[1]
                if not first then
                    return tostring(apps.available:get()) .. '|none'
                end
                local action = first.actions[1]
                return table.concat({
                    tostring(apps.available:get()),
                    first.id,
                    first.name,
                    first.generic_name,
                    first.icon,
                    table.concat(first.keywords, ','),
                    table.concat(first.categories, ','),
                    table.concat(first.command, ' '),
                    tostring(first.working_directory),
                    tostring(first.terminal),
                    tostring(first.no_display),
                    first.startup_wm_class,
                    action.key,
                    action.name,
                    table.concat(action.command, ' '),
                }, '|')
            end,
        } } }
        "#,
    );
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "false|none");

    runtime
        .set_applications(snapshot(vec![application("firefox.desktop", "Firefox")]))
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|firefox.desktop|Firefox|Web Browser|firefox|internet,web|Network|\
         /usr/lib/firefox/firefox|/tmp|false|false|firefox|new-private-window|\
         New Private Window|/usr/lib/firefox/firefox --private-window"
    );

    for mutation in [
        "services.applications.entries:get()[1].name = 'changed'",
        "table.insert(services.applications.entries:get()[1].command, '--evil')",
        "services.applications.entries:get()[1].actions[1].command[1] = 'sh'",
        "services.applications.entries = {}",
    ] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                {mutation}
            end }} }} }}
            "#,
        );
        let (_directory, mut mutated) = crate::runtime(&source);
        mutated
            .set_applications(snapshot(vec![application("firefox.desktop", "Firefox")]))
            .unwrap();
        mutated.refresh().unwrap();
        assert!(
            mutated
                .dispatch(mutated.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {mutation}"
        );
    }
}

#[test]
fn invalid_application_lists_preserve_the_previous_state() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                return tostring(#services.applications.entries:get())
            end,
        } } }
        "#,
    );
    runtime
        .set_applications(snapshot(vec![application("a.desktop", "A")]))
        .unwrap();
    runtime.refresh().unwrap();
    let duplicate = vec![application("a.desktop", "A"), application("a.desktop", "B")];
    let mut unnamed = application("b.desktop", "");
    unnamed.name.clear();
    let mut empty_program = application("c.desktop", "C");
    empty_program.command = vec![String::new()];
    let mut nul = application("d.desktop", "D");
    nul.keywords = vec!["bad\0keyword".into()];
    let mut oversized_command = application("large.desktop", "Large");
    oversized_command.command.extend(vec!["a".repeat(4096); 17]);
    let mut empty_directory = application("empty-directory.desktop", "Empty directory");
    empty_directory.working_directory = Some(String::new());
    for invalid in [
        duplicate,
        vec![unnamed],
        vec![empty_program],
        vec![nul],
        vec![oversized_command],
        vec![empty_directory],
    ] {
        assert!(runtime.set_applications(snapshot(invalid)).is_err());
    }
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().unwrap().children[0].text, "1");

    // an entry without a command describes an application it cannot launch.
    let mut descriptive = application("org.quickshell.desktop", "Quickshell");
    descriptive.command.clear();
    descriptive.no_display = true;
    runtime
        .set_applications(snapshot(vec![descriptive]))
        .unwrap();
}

#[test]
fn entries_launch_through_the_existing_launch_action() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            local app = services.applications.entries:get()[1]
            services.launch(app.command, app.working_directory)
            services.launch(app.actions[1].command)
        end } } }
        "#,
    );
    runtime
        .set_applications(snapshot(vec![application("firefox.desktop", "Firefox")]))
        .unwrap();
    runtime.refresh().unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        [
            Action::Launch {
                argv: vec!["/usr/lib/firefox/firefox".into()],
                cwd: Some("/tmp".into()),
            },
            Action::Launch {
                argv: vec!["/usr/lib/firefox/firefox".into(), "--private-window".into(),],
                cwd: None,
            },
        ]
    );
}
