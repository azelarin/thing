use std::fs;

use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(&path);
    (directory, runtime)
}

fn image(properties: &str) -> mlua::Result<thing_core::Image> {
    let (_directory, runtime) = load(&format!(
        "return ui.panel {{ children = {{ ui.image {{ {properties} }} }} }}"
    ));
    Ok(runtime?.root().unwrap().children[0].image.clone())
}

#[test]
fn image_sources_accept_urls_and_a_reactive_revision() {
    let remote =
        image("source = 'https://art.example/cover.jpg?size=large', revision = 7").unwrap();
    assert_eq!(
        remote.source.as_deref(),
        Some("https://art.example/cover.jpg?size=large")
    );
    assert_eq!(remote.revision.as_deref(), Some("7"));
    let named = image("source = 'file:///tmp/cover%20art.png', revision = 'track-2'").unwrap();
    assert_eq!(named.revision.as_deref(), Some("track-2"));
    assert_eq!(image("source = '/tmp/a.png'").unwrap().revision, None);

    let (_directory, runtime) = load(
        r#"
        local track = ui.state(1)
        return ui.panel { children = {
            ui.image { source = 'http://art.example/now.png', revision = function() return track:get() end },
            ui.button { on_click = function() track:set(track:get() + 1) end },
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[1].id;
    runtime.dispatch(button, Event::Click).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0]
            .image
            .revision
            .as_deref(),
        Some("2")
    );
}

#[test]
fn malformed_urls_and_revisions_are_rejected() {
    for (properties, message) in [
        ("source = 'https://'", "need a host"),
        ("source = 'http:///path.png'", "need a host"),
        ("source = 'https://art.example/a b.png'", "without spaces"),
        ("source = 'https://user@/a.png'", "need a host"),
        ("revision = 1.5", "whole number"),
        ("revision = true", "whole number"),
        (
            &format!("revision = '{}'", "x".repeat(257)),
            "at most 256 bytes",
        ),
    ] {
        let error = image(properties).err().unwrap().to_string();
        assert!(error.contains(message), "{properties}: {error}");
    }
    // a revision belongs to images only.
    let (_directory, runtime) = load("return ui.panel { revision = 1 }");
    assert!(runtime.is_err());
}
