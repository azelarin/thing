use std::{fs, sync::Arc};

use thing_runtime::{Action, FileWatchSnapshot, MAX_WATCHED_FILE_BYTES, Runtime};

fn load(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

#[test]
fn paths_belong_to_the_declaring_module_and_do_not_become_reload_dependencies() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("modules")).unwrap();
    fs::write(directory.path().join("modules/provider.luau"), "services.watch_file { key='data', path='./missing.txt', on_change=function() end }\nreturn {}").unwrap();
    let entry = directory.path().join("panel.luau");
    fs::write(&entry, "require('./modules/provider')\nreturn ui.panel {}").unwrap();
    let runtime = Runtime::load(entry).unwrap();
    let definitions = runtime.file_watches();
    assert_eq!(definitions.len(), 1);
    let data = directory.path().join("modules/missing.txt");
    assert_eq!(definitions[0].path, data.to_str().unwrap());
    assert!(!runtime.files().contains(&data));
    assert!(!data.exists());
}

const CALLBACK: &str = r#"
    local display = ui.state('waiting')
    services.watch_file {
        key='data', path='./data.txt',
        on_change=function(event)
            assert(not pcall(function() event.status='changed' end))
            if event.status=='ready' then
                display:set(event.text)
                services.launch { 'helper', event.text }
                if event.text=='bad' then error('rejected data') end
            else
                display:set(event.status)
            end
        end,
    }
    return ui.panel { children={ ui.text { text=function() return display:get() end } } }
"#;

fn text(runtime: &Runtime) -> &str {
    &runtime.root().unwrap().children[0].text
}

#[test]
fn snapshots_run_checked_callbacks_and_failed_callbacks_discard_actions() {
    let (_directory, mut runtime) = load(CALLBACK);
    assert_eq!(text(&runtime), "waiting");
    runtime
        .dispatch_file_watch_checked("data", &FileWatchSnapshot::Ready(Arc::from("good")), |_| {
            Ok(())
        })
        .unwrap();
    assert_eq!(text(&runtime), "good");
    assert!(
        matches!(&runtime.take_actions()[..], [Action::Launch { argv, .. }] if argv==&["helper","good"])
    );
    let result = runtime.dispatch_file_watch_checked(
        "data",
        &FileWatchSnapshot::Ready(Arc::from("bad")),
        |_| Ok(()),
    );
    assert!(
        result
            .expect_err("expected rejection")
            .to_string()
            .contains("rejected data")
    );
    assert_eq!(text(&runtime), "good");
    assert!(runtime.take_actions().is_empty());
    runtime
        .dispatch_file_watch_checked("data", &FileWatchSnapshot::Missing, |_| Ok(()))
        .unwrap();
    assert_eq!(text(&runtime), "missing");
    runtime
        .dispatch_file_watch_checked(
            "data",
            &FileWatchSnapshot::Failed(Arc::from("unreadable")),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(text(&runtime), "failed");
}

#[test]
fn a_rejected_layout_keeps_the_old_tree_and_another_snapshot_can_recover() {
    let (_directory, mut runtime) = load(CALLBACK);
    let result = runtime.dispatch_file_watch_checked(
        "data",
        &FileWatchSnapshot::Ready(Arc::from("first")),
        |_| Err("layout rejected".into()),
    );
    assert!(result.is_err());
    assert_eq!(text(&runtime), "waiting");
    assert!(runtime.take_actions().is_empty());
    runtime
        .dispatch_file_watch_checked(
            "data",
            &FileWatchSnapshot::Ready(Arc::from("later")),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(text(&runtime), "later");
}

#[test]
fn snapshots_are_bounded_before_the_callback_runs() {
    let (_directory, mut runtime) = load(
        "local n=ui.state(0)\nservices.watch_file {key='data',path='./data.txt',on_change=function(event) n:set(#event.text) end}\nreturn ui.panel {children={ui.text{text=function() return tostring(n:get()) end}}}",
    );
    for snapshot in [
        FileWatchSnapshot::Ready(Arc::from("a\0b")),
        FileWatchSnapshot::Ready(Arc::from("x".repeat(MAX_WATCHED_FILE_BYTES + 1))),
        FileWatchSnapshot::Failed(Arc::from("x".repeat(1025))),
    ] {
        assert!(
            runtime
                .dispatch_file_watch_checked("data", &snapshot, |_| Ok(()))
                .is_err()
        );
        assert_eq!(text(&runtime), "0");
    }
    runtime
        .dispatch_file_watch_checked(
            "data",
            &FileWatchSnapshot::Ready(Arc::from("x".repeat(MAX_WATCHED_FILE_BYTES))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(text(&runtime), MAX_WATCHED_FILE_BYTES.to_string());
}

#[test]
fn declarations_are_unique_load_only_and_unavailable_in_locks() {
    for options in [
        "{key='',path='./data',on_change=function() end}",
        "{key='data',path='data',on_change=function() end}",
        "{key='data',path='./data',on_change=1}",
        "{key='data',path='./data',extra=true,on_change=function() end}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            format!("services.watch_file {options}\nreturn ui.panel {{}}"),
        )
        .unwrap();
        assert!(Runtime::load(path).is_err(), "{options}");
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path,"for i=1,2 do services.watch_file {key='same',path='./data',on_change=function() end} end\nreturn ui.panel {}").unwrap();
    assert!(
        Runtime::load(&path)
            .err()
            .expect("expected rejection")
            .to_string()
            .contains("unique")
    );
    fs::write(&path,"return ui.panel {children={ui.text {text=function() services.watch_file {key='late',path='./data',on_change=function() end} return '' end}}}").unwrap();
    assert!(
        Runtime::load(&path)
            .err()
            .expect("expected rejection")
            .to_string()
            .contains("while the configuration loads")
    );
    fs::write(&path,"services.watch_file {key='data',path='./data',on_change=function() end}\nreturn ui.lock {children={ui.secret_input{}}}").unwrap();
    assert!(
        Runtime::load_lock(&path)
            .err()
            .expect("expected rejection")
            .to_string()
            .contains("unavailable in lock")
    );
}

#[cfg(unix)]
#[test]
fn parent_components_follow_filesystem_semantics_after_a_symlink() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("real/child")).unwrap();
    fs::write(directory.path().join("real/data"), "correct").unwrap();
    fs::write(directory.path().join("data"), "wrong").unwrap();
    symlink(
        directory.path().join("real/child"),
        directory.path().join("alias"),
    )
    .unwrap();
    let entry = directory.path().join("panel.luau");
    fs::write(&entry,"services.watch_file {key='data',path='./alias/../data',on_change=function() end}\nreturn ui.panel {}").unwrap();
    let runtime = Runtime::load(entry).unwrap();
    assert_eq!(
        fs::read_to_string(&runtime.file_watches()[0].path).unwrap(),
        "correct"
    );
}
