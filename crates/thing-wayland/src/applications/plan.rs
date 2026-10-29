//! turns a launch request into processes to start or a d-bus activation.

use thing_runtime::{ApplicationLaunch, DesktopArguments, is_local_file, uri_scheme};
use zbus::{names::WellKnownName, zvariant::ObjectPath};

use super::{
    Catalog,
    activation::{ActivationCall, Method},
    entry::{Entry, Files, Template, valid_command},
};
use crate::launch::LaunchRequest;

/// the most processes one launch starts for `%f` or `%u`, one per file.
const MAX_PROCESSES: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Spawn(Vec<LaunchRequest>),
    Activate(ActivationCall),
}

/// resolves a launch against the catalog that matches the list the
/// configuration saw. the request is refused when the entry has since changed.
pub(crate) fn plan(catalog: &Catalog, launch: &ApplicationLaunch) -> Result<Plan, String> {
    let id = &launch.id;
    let entry = catalog
        .get(id)
        .ok_or_else(|| format!("application {id:?} is no longer installed"))?;
    if entry.application.revision != launch.revision {
        return Err(format!(
            "application {id:?} changed after the configuration saw it; launch it again"
        ));
    }
    if launch.action.is_some() && !launch.uris.is_empty() {
        return Err("application actions do not accept uris".into());
    }
    let template = match &launch.action {
        Some(key) => entry
            .actions
            .get(key)
            .ok_or_else(|| format!("application {id:?} has no action {key:?}"))?
            .as_ref(),
        None => entry.exec.as_ref(),
    };
    let targets = launch
        .uris
        .iter()
        .map(|uri| Target::parse(uri))
        .collect::<Result<Vec<_>, _>>()?;
    let commands = commands(entry, template, &targets, &launch.terminal);
    if entry.application.dbus_activatable
        && let Some((bus_name, path)) = dbus_address(id)
    {
        let method = match &launch.action {
            Some(key) => Method::ActivateAction(key.clone()),
            None if targets.is_empty() => Method::Activate,
            None => Method::Open(targets.into_iter().map(|target| target.uri).collect()),
        };
        return Ok(Plan::Activate(ActivationCall {
            id: id.clone(),
            bus_name,
            path,
            method,
            fallback: commands,
        }));
    }
    commands.map(Plan::Spawn)
}

/// expands an `Exec` template into the processes to start.
fn commands(
    entry: &Entry,
    template: Option<&Template>,
    targets: &[Target],
    terminal: &[String],
) -> Result<Vec<LaunchRequest>, String> {
    let application = &entry.application;
    let id = &application.id;
    let template = template.ok_or_else(|| format!("application {id:?} has no Exec command"))?;
    let accepts = template.accepts();
    if !targets.is_empty() && accepts == DesktopArguments::None {
        return Err(format!(
            "the Exec command of {id:?} does not accept files or urls"
        ));
    }
    let values = targets
        .iter()
        .map(|target| match accepts {
            DesktopArguments::File | DesktopArguments::Files => {
                target.path.clone().ok_or_else(|| {
                    format!(
                        "{id:?} opens only local files, and {:?} is not one; remote files are not downloaded",
                        target.uri
                    )
                })
            }
            _ => Ok(target.uri.clone()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expanded = match accepts {
        DesktopArguments::File | DesktopArguments::Url if values.len() > MAX_PROCESSES => {
            return Err(format!(
                "{id:?} takes one file per process, so one launch accepts at most {MAX_PROCESSES}"
            ));
        }
        // a singular code starts one process for each file.
        DesktopArguments::File | DesktopArguments::Url if !values.is_empty() => values
            .iter()
            .map(|value| template.expand(Files::One(value)))
            .collect::<Option<Vec<_>>>(),
        DesktopArguments::Files | DesktopArguments::Urls => template
            .expand(Files::All(&values))
            .map(|command| vec![command]),
        _ => template.expand(Files::None).map(|command| vec![command]),
    }
    .ok_or_else(|| format!("the expanded command of {id:?} exceeds the launch limits"))?;
    let mut requests = Vec::with_capacity(expanded.len());
    for command in expanded {
        let argv = if application.terminal {
            if terminal.is_empty() {
                return Err(format!(
                    "{id:?} runs in a terminal; pass a terminal command in options.terminal"
                ));
            }
            let argv = [terminal, &command].concat();
            if !valid_command(&argv) {
                return Err(format!(
                    "the terminal command for {id:?} exceeds the launch limits"
                ));
            }
            argv
        } else {
            command
        };
        requests.push(LaunchRequest {
            argv,
            cwd: application.working_directory.clone(),
        });
    }
    Ok(requests)
}

/// derives the well-known name and object path the specification assigns to
/// a desktop file id, or `None` when the id cannot name a d-bus service.
fn dbus_address(id: &str) -> Option<(String, String)> {
    let name = id.strip_suffix(".desktop")?;
    WellKnownName::try_from(name).ok()?;
    let path = format!("/{}", name.replace('.', "/").replace('-', "_"));
    ObjectPath::try_from(path.as_str()).ok()?;
    Some((name.to_owned(), path))
}

/// one file or url to open, as a uri and, for a local file, as a path.
#[derive(Debug)]
struct Target {
    uri: String,
    path: Option<String>,
}

impl Target {
    fn parse(value: &str) -> Result<Self, String> {
        if value.starts_with('/') {
            return Ok(Self {
                uri: file_uri(value),
                path: Some(value.to_owned()),
            });
        }
        if uri_scheme(value).is_none() {
            return Err(format!("{value:?} is neither an absolute path nor a uri"));
        }
        if value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(format!("the uri {value:?} must be percent-encoded"));
        }
        Ok(Self {
            uri: value.to_owned(),
            path: local_path(value)?,
        })
    }
}

/// decodes a local `file:` uri into a path, or returns `None` for a remote uri.
fn local_path(uri: &str) -> Result<Option<String>, String> {
    if !is_local_file(uri) {
        return Ok(None);
    }
    let rest = &uri["file:".len()..];
    let path = match rest.strip_prefix("//") {
        Some(authority) => &authority[authority.find('/').expect("a local file uri has a path")..],
        None => rest,
    };
    if path.contains(['?', '#']) {
        return Err(format!(
            "the file uri {uri:?} has a query or fragment, so it names no file"
        ));
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = input.next().and_then(hex);
        let low = input.next().and_then(hex);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(format!(
                "the file uri {uri:?} has an invalid percent escape"
            ));
        };
        bytes.push(high << 4 | low);
    }
    let path = String::from_utf8(bytes)
        .map_err(|_| format!("the file uri {uri:?} does not decode to a utf-8 path"))?;
    if path.contains('\0') {
        return Err(format!("the file uri {uri:?} decodes to a path with NUL"));
    }
    Ok(Some(path))
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// encodes an absolute path as a `file:` uri, escaping everything except
/// unreserved characters and separators.
fn file_uri(path: &str) -> String {
    let mut uri = String::with_capacity(path.len() + "file://".len());
    uri.push_str("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        applications::entry::{Environment, parse},
        launch::Launcher,
    };

    fn catalog(entries: &[(&str, &str)]) -> Catalog {
        let environment = Environment {
            desktops: vec!["niri".into()],
            ..Environment::default()
        };
        let entries = entries
            .iter()
            .enumerate()
            .map(|(index, (id, source))| {
                let path = Path::new("/usr/share/applications").join(id);
                let mut entry = parse(id, &path, source, &environment).unwrap();
                entry.application.revision = index as u64 + 1;
                (id.to_string(), entry)
            })
            .collect();
        Catalog { entries }
    }

    fn launch(catalog: &Catalog, id: &str) -> ApplicationLaunch {
        ApplicationLaunch {
            id: id.into(),
            revision: catalog.get(id).unwrap().application.revision,
            action: None,
            uris: Vec::new(),
            terminal: Vec::new(),
        }
    }

    fn with_uris(mut launch: ApplicationLaunch, uris: &[&str]) -> ApplicationLaunch {
        launch.uris = uris.iter().map(|uri| (*uri).into()).collect();
        launch
    }

    fn spawned(plan: Result<Plan, String>) -> Vec<Vec<String>> {
        match plan.unwrap() {
            Plan::Spawn(requests) => requests.into_iter().map(|request| request.argv).collect(),
            Plan::Activate(call) => panic!("expected processes, got {call:?}"),
        }
    }

    fn entry(exec: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName=Viewer\nIcon=viewer\nExec={exec}\n")
    }

    #[test]
    fn field_codes_expand_per_launch_with_singular_and_plural_semantics() {
        let catalog = catalog(&[
            ("single.desktop", &entry("view --file=%f %c")),
            ("many.desktop", &entry(r#"view "--title=a b" %F %i"#)),
            ("url.desktop", &entry("browse %u")),
            ("urls.desktop", &entry("browse --new %U")),
            ("plain.desktop", &entry("plain %k")),
        ]);
        let files = ["/tmp/a b.txt", "file:///tmp/100%25%20done%C3%A9.txt"];

        // %f starts one process per file and decodes file uris to paths.
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(launch(&catalog, "single.desktop"), &files)
            )),
            [
                ["view", "--file=/tmp/a b.txt", "Viewer"],
                ["view", "--file=/tmp/100% doneé.txt", "Viewer"],
            ]
        );
        // without files, the codes disappear, matching the listed command.
        assert_eq!(
            spawned(plan(&catalog, &launch(&catalog, "single.desktop"))),
            [["view", "--file=", "Viewer"]]
        );
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(launch(&catalog, "many.desktop"), &files)
            )),
            [[
                "view",
                "--title=a b",
                "/tmp/a b.txt",
                "/tmp/100% doneé.txt",
                "--icon",
                "viewer",
            ]]
        );
        // url codes keep uris as given and encode paths as file uris.
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(
                    launch(&catalog, "url.desktop"),
                    &["https://example.org/?q=a%20b", "/tmp/a b"]
                )
            )),
            [
                vec!["browse", "https://example.org/?q=a%20b"],
                vec!["browse", "file:///tmp/a%20b"],
            ]
        );
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(
                    launch(&catalog, "urls.desktop"),
                    &["https://example.org", "/tmp/x"]
                )
            )),
            [["browse", "--new", "https://example.org", "file:///tmp/x"]]
        );
        assert_eq!(
            spawned(plan(&catalog, &launch(&catalog, "plain.desktop"))),
            [["plain", "/usr/share/applications/plain.desktop"]]
        );
    }

    #[test]
    fn local_file_hosts_are_case_insensitive() {
        let catalog = catalog(&[("files.desktop", &entry("view %F"))]);
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(
                    launch(&catalog, "files.desktop"),
                    &["FILE://LOCALHOST/tmp/A%20B"]
                )
            )),
            [["view", "/tmp/A B"]]
        );
    }

    #[test]
    fn unsupported_files_are_refused_rather_than_ignored_or_downloaded() {
        let catalog = catalog(&[
            ("single.desktop", &entry("view %f")),
            ("plain.desktop", &entry("plain")),
            ("twice.desktop", &entry("view %f %u")),
        ]);
        let refused = |id: &str, uris: &[&str]| {
            plan(&catalog, &with_uris(launch(&catalog, id), uris)).unwrap_err()
        };
        assert!(refused("plain.desktop", &["/tmp/a"]).contains("does not accept"));
        // two file codes make the entry accept no files at all.
        assert!(refused("twice.desktop", &["/tmp/a"]).contains("does not accept"));
        assert!(
            refused("single.desktop", &["https://example.org/a.png"]).contains("not downloaded")
        );
        assert!(refused("single.desktop", &["file://server/a.png"]).contains("not downloaded"));
        for (uri, reason) in [
            ("file:///tmp/%zz", "percent escape"),
            ("file:///tmp/%2", "percent escape"),
            ("file:///tmp/%FF", "utf-8"),
            ("file:///tmp/%00", "NUL"),
            ("file:///tmp/a?b", "query"),
            ("https://example.org/a b", "percent-encoded"),
            ("relative/path", "absolute path"),
        ] {
            assert!(refused("single.desktop", &[uri]).contains(reason), "{uri}");
        }
        let many = vec!["/tmp/a"; MAX_PROCESSES + 1];
        assert!(refused("single.desktop", &many).contains("at most"));
        assert_eq!(
            spawned(plan(
                &catalog,
                &with_uris(launch(&catalog, "single.desktop"), &many[1..])
            ))
            .len(),
            MAX_PROCESSES
        );
    }

    #[test]
    fn terminal_prefixes_and_working_directories_apply_only_to_exec_launches() {
        let catalog = catalog(&[
            (
                "top.desktop",
                "[Desktop Entry]\nType=Application\nName=Top\nExec=top -d 1\nTerminal=true\nPath=/srv/work dir\n",
            ),
            ("gui.desktop", &entry("view")),
        ]);
        let error = plan(&catalog, &launch(&catalog, "top.desktop")).unwrap_err();
        assert!(error.contains("options.terminal"), "{error}");
        let mut request = launch(&catalog, "top.desktop");
        request.terminal = vec!["foot".into(), "-e".into()];
        let Plan::Spawn(requests) = plan(&catalog, &request).unwrap() else {
            panic!("expected a process");
        };
        assert_eq!(
            requests,
            [LaunchRequest {
                argv: vec![
                    "foot".into(),
                    "-e".into(),
                    "top".into(),
                    "-d".into(),
                    "1".into()
                ],
                cwd: Some("/srv/work dir".into()),
            }]
        );
        // a graphical entry ignores the terminal the configuration passes.
        let mut request = launch(&catalog, "gui.desktop");
        request.terminal = vec!["foot".into(), "-e".into()];
        assert_eq!(spawned(plan(&catalog, &request)), [["view"]]);
    }

    #[test]
    fn launches_bind_to_the_observed_entry_and_its_actions() {
        let catalog = catalog(&[(
            "editor.desktop",
            "[Desktop Entry]\nType=Application\nName=Editor\nExec=edit %F\nActions=new;\n\
             [Desktop Action new]\nName=New\nExec=edit --new\n",
        )]);
        let mut stale = launch(&catalog, "editor.desktop");
        stale.revision += 1;
        assert!(plan(&catalog, &stale).unwrap_err().contains("changed"));
        let mut removed = launch(&catalog, "editor.desktop");
        removed.id = "gone.desktop".into();
        assert!(
            plan(&catalog, &removed)
                .unwrap_err()
                .contains("no longer installed")
        );
        let mut action = launch(&catalog, "editor.desktop");
        action.action = Some("new".into());
        assert_eq!(spawned(plan(&catalog, &action)), [["edit", "--new"]]);
        action.uris = vec!["/tmp/a".into()];
        assert!(
            plan(&catalog, &action)
                .unwrap_err()
                .contains("do not accept uris")
        );
        action.uris.clear();
        action.action = Some("missing".into());
        assert!(plan(&catalog, &action).unwrap_err().contains("no action"));
    }

    #[test]
    fn dbus_activatable_entries_derive_their_address_and_keep_an_exec_fallback() {
        let source = "[Desktop Entry]\nType=Application\nName=Files\nDBusActivatable=true\n\
                      Exec=files %U\nActions=window;\n[Desktop Action window]\nName=Window\n";
        let catalog = catalog(&[
            ("org.example.my-files.desktop", source),
            ("files.desktop", source),
            (
                "org.example.Service.desktop",
                "[Desktop Entry]\nType=Application\nName=Service\nDBusActivatable=true\n",
            ),
        ]);
        let id = "org.example.my-files.desktop";
        let call = |request: ApplicationLaunch| match plan(&catalog, &request).unwrap() {
            Plan::Activate(call) => call,
            Plan::Spawn(requests) => panic!("expected activation, got {requests:?}"),
        };
        let activate = call(launch(&catalog, id));
        assert_eq!(activate.bus_name, "org.example.my-files");
        assert_eq!(activate.path, "/org/example/my_files");
        assert_eq!(activate.method, Method::Activate);
        assert_eq!(
            activate.fallback,
            Ok(vec![LaunchRequest {
                argv: vec!["files".into()],
                cwd: None,
            }])
        );
        let open = call(with_uris(
            launch(&catalog, id),
            &["/tmp/a b", "https://example.org"],
        ));
        assert_eq!(
            open.method,
            Method::Open(vec![
                "file:///tmp/a%20b".into(),
                "https://example.org".into()
            ])
        );
        assert_eq!(
            open.fallback.unwrap()[0].argv,
            ["files", "file:///tmp/a%20b", "https://example.org"]
        );
        let mut request = launch(&catalog, id);
        request.action = Some("window".into());
        let action = call(request);
        assert_eq!(action.method, Method::ActivateAction("window".into()));
        assert!(action.fallback.unwrap_err().contains("no Exec"));
        // an id that is not a valid bus name cannot be activated over d-bus.
        assert_eq!(
            spawned(plan(&catalog, &launch(&catalog, "files.desktop"))),
            [["files"]]
        );
        assert!(
            call(launch(&catalog, "org.example.Service.desktop"))
                .fallback
                .unwrap_err()
                .contains("no Exec")
        );
    }

    #[test]
    fn expanded_arguments_reach_a_process_without_a_shell() {
        let directory = tempfile::tempdir().unwrap();
        let work = directory.path().join("work dir");
        fs::create_dir(&work).unwrap();
        let output = directory.path().join("output");
        let helper = directory.path().join("helper");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\n{{ printf 'cwd=<%s>\\n' \"$PWD\"; for arg do printf 'arg=<%s>\\n' \"$arg\"; done; }} > '{}'\n",
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let source = format!(
            "[Desktop Entry]\nType=Application\nName=Helper\nPath={}\nExec=\"{}\" \"--literal=$HOME;`x`\" %F\n",
            work.display(),
            helper.display()
        );
        let catalog = catalog(&[("helper.desktop", &source)]);
        let request = with_uris(
            launch(&catalog, "helper.desktop"),
            &["/tmp/it's here", "file:///tmp/semi%3Bcolon%20%24HOME"],
        );
        let Plan::Spawn(requests) = plan(&catalog, &request).unwrap() else {
            panic!("expected a process");
        };
        let mut launcher = Launcher::for_tests();
        for request in &requests {
            launcher
                .spawn(&request.argv, request.cwd.as_deref(), None)
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(&output)
            .is_ok_and(|text| text.ends_with(">\n") && text.matches("arg=").count() == 3)
        {
            assert!(Instant::now() < deadline, "the helper did not run");
            thread::sleep(Duration::from_millis(10));
        }
        let output = fs::read_to_string(&output).unwrap();
        assert_eq!(
            output,
            format!(
                "cwd=<{}>\narg=<--literal=$HOME;`x`>\narg=</tmp/it's here>\narg=</tmp/semi;colon $HOME>\n",
                work.display()
            )
        );
        while !launcher.reap().is_empty() {}
    }
}
