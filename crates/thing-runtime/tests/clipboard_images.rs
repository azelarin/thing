use std::fs;

use tempfile::TempDir;
use thing_core::{ImageCandidate, ImagePixmap, ImageSource};
use thing_runtime::{Action, ClipboardImageStatus, ClipboardSnapshot, Event, Runtime};

fn load(source: &str) -> (TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn image(id: &str) -> ImageSource {
    let pixmap = ImagePixmap::new(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    ImageSource::new(
        id.into(),
        vec![ImageCandidate::new(None, vec![pixmap]).unwrap()],
    )
    .unwrap()
}

fn ready(id: &str) -> ClipboardSnapshot {
    ClipboardSnapshot {
        available: true,
        formats: vec!["image/png".into(), "text/html".into()],
        image_status: ClipboardImageStatus::Ready,
        image: Some(image(id)),
        ..ClipboardSnapshot::default()
    }
}

const PANEL: &str = r#"
    local clipboard = services.clipboard
    return ui.panel { children = {
        ui.text { text = function()
            return `{clipboard.image_status:get()} {table.concat(clipboard.formats:get(), ',')} {clipboard.image_error:get()}`
        end },
        ui.image { source = function()
            return clipboard.image:get() or ui.asset('./placeholder.png')
        end },
        ui.button { on_click = function() clipboard.read_image() end },
        ui.button { on_click = function()
            local image = clipboard.image:get()
            if image then clipboard.set_image(image) end
        end },
        ui.button { on_click = function() clipboard.set_image('/tmp/not-a-handle.png') end },
    } }
"#;

#[test]
fn image_status_formats_and_handles_follow_snapshots() {
    let (_directory, mut runtime) = load(PANEL);
    let text = |runtime: &Runtime| runtime.root().unwrap().children[0].text.clone();
    let memory = |runtime: &Runtime| runtime.root().unwrap().children[1].image.memory.clone();
    assert_eq!(text(&runtime), "unavailable  ");
    assert_eq!(memory(&runtime), None);

    runtime
        .set_clipboard(ClipboardSnapshot {
            available: true,
            formats: vec!["image/jpeg".into()],
            image_status: ClipboardImageStatus::Unsupported,
            ..ClipboardSnapshot::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "unsupported image/jpeg ");

    runtime.set_clipboard(ready("clipboard-image:1")).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "ready image/png,text/html ");
    // the handle is an ordinary in-memory image source for ui.image.
    assert_eq!(memory(&runtime), Some(image("clipboard-image:1")));

    runtime
        .set_clipboard(ClipboardSnapshot {
            available: true,
            formats: vec!["image/png".into()],
            image_status: ClipboardImageStatus::Failed,
            image_error: "cannot decode the clipboard image".into(),
            ..ClipboardSnapshot::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        text(&runtime),
        "failed image/png cannot decode the clipboard image"
    );
    assert_eq!(memory(&runtime), None);
}

#[test]
fn image_actions_are_explicit_and_need_handles() {
    let (_directory, mut runtime) = load(PANEL);
    runtime.set_clipboard(ready("clipboard-image:2")).unwrap();
    runtime.refresh().unwrap();
    let buttons: Vec<_> = runtime.root().unwrap().children[2..]
        .iter()
        .map(|node| node.id)
        .collect();
    runtime.dispatch(buttons[0], Event::Click).unwrap();
    assert_eq!(runtime.take_actions(), [Action::ReadClipboardImage]);
    runtime.dispatch(buttons[1], Event::Click).unwrap();
    assert_eq!(
        runtime.take_actions(),
        [Action::SetClipboardImage(image("clipboard-image:2"))]
    );
    let error = runtime
        .dispatch(buttons[2], Event::Click)
        .unwrap_err()
        .to_string();
    assert!(error.contains("set_image needs an image handle"), "{error}");
    assert!(runtime.take_actions().is_empty());

    for source in [
        "services.clipboard.read_image() return ui.panel {}",
        "return ui.panel { children = { ui.text { text = function() services.clipboard.read_image() return '' end } } }",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        let error = Runtime::load(path).err().unwrap().to_string();
        assert!(error.contains("only in event handlers"), "{error}");
    }
}

#[test]
fn handles_beyond_the_png_limits_are_rejected_without_queuing() {
    let (_directory, mut runtime) = load(PANEL);
    let wide = thing_runtime::MAX_CLIPBOARD_IMAGE_SIDE + 1;
    let pixmap = ImagePixmap::new(wide, 1, vec![0; wide as usize * 4]).unwrap();
    let small = ImagePixmap::new(1, 1, vec![0; 4]).unwrap();
    // the largest raster is the one offered, so a small companion does not help.
    let image = ImageSource::new(
        "clipboard-image:wide".into(),
        vec![ImageCandidate::new(None, vec![small, pixmap]).unwrap()],
    )
    .unwrap();
    runtime
        .set_clipboard(ClipboardSnapshot {
            available: true,
            formats: vec!["image/png".into()],
            image_status: ClipboardImageStatus::Ready,
            image: Some(image),
            ..ClipboardSnapshot::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    let copy = runtime.root().unwrap().children[3].id;
    let error = runtime
        .dispatch(copy, Event::Click)
        .unwrap_err()
        .to_string();
    assert!(error.contains("limited"), "{error}");
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn inconsistent_image_snapshots_are_rejected_and_replacements_keep_the_image() {
    let (directory, mut runtime) = load(PANEL);
    for snapshot in [
        ClipboardSnapshot {
            available: true,
            image_status: ClipboardImageStatus::Ready,
            ..ClipboardSnapshot::default()
        },
        ClipboardSnapshot {
            available: true,
            image_status: ClipboardImageStatus::Available,
            image: Some(image("x")),
            ..ClipboardSnapshot::default()
        },
        ClipboardSnapshot {
            available: true,
            image_status: ClipboardImageStatus::Failed,
            ..ClipboardSnapshot::default()
        },
        ClipboardSnapshot {
            formats: vec!["image/png".into()],
            ..ClipboardSnapshot::default()
        },
        ClipboardSnapshot {
            available: true,
            formats: vec!["x".repeat(300)],
            ..ClipboardSnapshot::default()
        },
        ClipboardSnapshot {
            available: true,
            formats: vec!["image/png".into(); 65],
            ..ClipboardSnapshot::default()
        },
    ] {
        assert!(
            runtime.set_clipboard(snapshot.clone()).is_err(),
            "{snapshot:?}"
        );
    }
    runtime.set_clipboard(ready("clipboard-image:3")).unwrap();
    runtime.refresh().unwrap();
    let replacement = runtime
        .load_replacement(directory.path().join("panel.luau"))
        .unwrap();
    assert_eq!(
        replacement.root().unwrap().children[1].image.memory,
        Some(image("clipboard-image:3"))
    );
    assert_eq!(
        replacement.root().unwrap().children[0].text,
        "ready image/png,text/html "
    );
}
