use std::fs;

use thing_runtime::{
    Action, ApplicationLaunch, ApplicationsSnapshot, DesktopAction, DesktopApplication,
    DesktopArguments, Event, KeyInput, Runtime,
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
        revision: 7,
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
        accepts: DesktopArguments::Urls,
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

fn click(source: &str, applications: Vec<DesktopApplication>) -> (Runtime, mlua::Result<()>) {
    let (directory, mut runtime) = runtime(source);
    runtime.set_applications(snapshot(applications)).unwrap();
    runtime.refresh().unwrap();
    let id = runtime.root().unwrap().children[0].id;
    let result = runtime.dispatch(id, Event::Click).map(|_| ());
    drop(directory);
    (runtime, result)
}

fn button(body: &str) -> String {
    format!(
        "return ui.panel {{ children = {{ ui.button {{ on_click = function()\n{body}\nend }} }} }}"
    )
}

#[test]
fn application_launches_carry_the_observed_revision_and_options() {
    let (mut runtime, result) = click(
        &button(
            r#"
            local apps = services.applications
            assert(apps.entries:get()[1].accepts == 'urls')
            apps.launch('firefox.desktop')
            apps.launch('firefox.desktop', { action = 'new-private-window' })
            apps.launch('firefox.desktop', {
                uris = { 'https://example.org/a%20b', '/tmp/a b.txt' },
                terminal = { 'foot', '-e' },
            })
            "#,
        ),
        vec![application("firefox.desktop", "Firefox")],
    );
    result.unwrap();
    let launch = |action: Option<&str>, uris: &[&str], terminal: &[&str]| {
        Action::LaunchApplication(ApplicationLaunch {
            id: "firefox.desktop".into(),
            revision: 7,
            action: action.map(Into::into),
            uris: uris.iter().map(|uri| (*uri).into()).collect(),
            terminal: terminal.iter().map(|argument| (*argument).into()).collect(),
        })
    };
    assert_eq!(
        runtime.take_actions(),
        [
            launch(None, &[], &[]),
            launch(Some("new-private-window"), &[], &[]),
            launch(
                None,
                &["https://example.org/a%20b", "/tmp/a b.txt"],
                &["foot", "-e"]
            ),
        ]
    );
}

#[test]
fn application_launches_reject_misuse_before_queueing() {
    let mut no_files = application("plain.desktop", "Plain");
    no_files.accepts = DesktopArguments::None;
    let mut local_only = application("viewer.desktop", "Viewer");
    local_only.accepts = DesktopArguments::File;
    let applications = vec![
        application("firefox.desktop", "Firefox"),
        no_files,
        local_only,
    ];
    for call in [
        "services.applications.launch('missing.desktop')",
        "services.applications.launch(42)",
        "services.applications.launch('firefox.desktop', 'new-private-window')",
        "services.applications.launch('firefox.desktop', { action = 'missing' })",
        "services.applications.launch('firefox.desktop', { unknown = true })",
        "services.applications.launch('firefox.desktop', { uris = 'https://example.org' })",
        "services.applications.launch('firefox.desktop', { uris = {} })",
        "services.applications.launch('firefox.desktop', { uris = { '' } })",
        "services.applications.launch('firefox.desktop', { uris = { 'relative.txt' } })",
        "services.applications.launch('firefox.desktop', { uris = { 'bad\\0nul' } })",
        "services.applications.launch('firefox.desktop', { terminal = 'foot' })",
        "services.applications.launch('firefox.desktop', { terminal = { '' } })",
        "services.applications.launch('firefox.desktop', \
            { action = 'new-private-window', uris = { '/tmp/a' } })",
        "services.applications.launch('plain.desktop', { uris = { '/tmp/a' } })",
        "services.applications.launch('viewer.desktop', { uris = { 'https://example.org/a.png' } })",
        "services.applications.launch('viewer.desktop', { uris = { 'file://host/a.png' } })",
    ] {
        let (mut runtime, result) = click(&button(call), applications.clone());
        assert!(result.is_err(), "accepted {call}");
        assert!(runtime.take_actions().is_empty(), "queued {call}");
    }
    // local files may be paths or file uris without a remote host.
    let (mut runtime, result) = click(
        &button(
            "services.applications.launch('viewer.desktop', \
             { uris = { '/tmp/a', 'file:///tmp/b', 'file://localhost/tmp/c', 'FILE://LOCALHOST/tmp/d' } })",
        ),
        applications,
    );
    result.unwrap();
    assert_eq!(runtime.take_actions().len(), 1);
}

#[test]
fn application_launches_are_event_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        return ui.panel { children = { ui.text { text = function()
            services.applications.launch('firefox.desktop')
            return 'launched'
        end } } }
        "#,
    )
    .unwrap();
    let error = Runtime::load(path).err().unwrap().to_string();
    assert!(error.contains("only in event handlers"), "{error}");
}

#[test]
fn lock_configurations_cannot_launch_applications() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    fs::write(
        &path,
        r#"
        return ui.lock {
            on_key = function()
                services.applications.launch('firefox.desktop')
                return true
            end,
            children = { ui.secret_input {} },
        }
        "#,
    )
    .unwrap();
    let mut runtime = Runtime::load_lock(path).unwrap();
    runtime
        .set_applications(snapshot(vec![application("firefox.desktop", "Firefox")]))
        .unwrap();
    let root = runtime.root().unwrap().id;
    let error = runtime
        .dispatch_key(
            root,
            KeyInput {
                key: "F1".into(),
                text: None,
                shift: false,
                ctrl: false,
                alt: false,
                logo: false,
                repeated: false,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("lock configurations"), "{error}");
    assert!(runtime.take_actions().is_empty());
}
