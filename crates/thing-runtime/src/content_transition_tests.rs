use std::{
    fs,
    time::{Duration, Instant},
};

use super::{Event, Runtime};
use tempfile::TempDir;
use thing_core::Node;

const PATTERNS: &str = include_str!("../../../tests/rendering/content-transitions/patterns.luau");
const GALLERY: &str = include_str!("../../../tests/rendering/content-transitions/gallery.luau");

fn fixture() -> (TempDir, Runtime, Instant) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("patterns.luau"), PATTERNS).unwrap();
    fs::write(directory.path().join("gallery.luau"), GALLERY).unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return require('./gallery')(false, false)").unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    let now = Instant::now() + Duration::from_secs(3600);
    runtime.activate_timers(now);
    (directory, runtime, now)
}

fn find<'a>(node: &'a Node, key: &str) -> &'a Node {
    fn search<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
        if node.key.as_deref() == Some(key) {
            return Some(node);
        }
        node.children.iter().find_map(|child| search(child, key))
    }
    search(node, key).unwrap()
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    find(runtime.root().unwrap(), key)
}

fn click(runtime: &mut Runtime, key: &str, now: Instant) {
    let id = node(runtime, key).id;
    runtime
        .with_event_controls(Some(now), |runtime| {
            runtime.dispatch_checked_inner(id, Event::Click, |_| Ok(()))
        })
        .unwrap();
}

fn tick(runtime: &mut Runtime, now: Instant) {
    runtime.advance(now).unwrap();
    for token in runtime.due_timers(now) {
        runtime
            .dispatch_timer_checked(&token, now, |_| Ok(()))
            .unwrap();
    }
}

#[test]
fn replacement_keeps_outgoing_content_until_hidden_and_uses_the_latest_request() {
    let (_directory, mut runtime, start) = fixture();
    let old = node(&runtime, "replacement").children[0].id;
    click(&mut runtime, "select-beta", start);
    tick(&mut runtime, start + Duration::from_millis(60));
    assert_eq!(node(&runtime, "shown").text, "alpha");
    assert_eq!(node(&runtime, "replacement").children[0].id, old);
    assert!(node(&runtime, "replacement").children[0].style.opacity < 1.0);
    click(
        &mut runtime,
        "select-gamma",
        start + Duration::from_millis(80),
    );
    tick(&mut runtime, start + Duration::from_millis(120));
    assert_eq!(node(&runtime, "shown").text, "alpha");
    assert_eq!(node(&runtime, "replacement").children[0].id, old);
    tick(&mut runtime, start + Duration::from_millis(200));
    assert_eq!(node(&runtime, "shown").text, "gamma");
    assert_ne!(node(&runtime, "replacement").children[0].id, old);
    assert_eq!(node(&runtime, "replacement").children[0].style.opacity, 0.0);
    tick(&mut runtime, start + Duration::from_millis(400));
    assert_eq!(node(&runtime, "phase").text, "idle");
    assert_eq!(node(&runtime, "replacement").children[0].style.opacity, 1.0);
    assert!(runtime.next_timer_deadline().is_none());
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn returning_to_outgoing_content_reverses_the_fade_without_replacing_its_key() {
    let (_directory, mut runtime, start) = fixture();
    let old = node(&runtime, "replacement").children[0].id;
    click(&mut runtime, "select-beta", start);
    tick(&mut runtime, start + Duration::from_millis(90));
    let opacity = node(&runtime, "replacement").children[0].style.opacity;
    click(
        &mut runtime,
        "select-alpha",
        start + Duration::from_millis(90),
    );
    assert_eq!(
        node(&runtime, "replacement").children[0].style.opacity,
        opacity
    );
    tick(&mut runtime, start + Duration::from_millis(180));
    assert!(node(&runtime, "replacement").children[0].style.opacity > opacity);
    assert_eq!(node(&runtime, "replacement").children[0].id, old);
    tick(&mut runtime, start + Duration::from_millis(300));
    assert_eq!(node(&runtime, "shown").text, "alpha");
    assert_eq!(node(&runtime, "requested").text, "alpha");
    assert!(runtime.next_timer_deadline().is_none());
}

#[test]
fn an_opaque_crossfade_preserves_both_keys_until_it_settles() {
    let (_directory, mut runtime, start) = fixture();
    let left = node(&runtime, "crossfade").children[0].id;
    click(&mut runtime, "blend-beta", start);
    tick(&mut runtime, start + Duration::from_millis(150));
    let children = &node(&runtime, "crossfade").children;
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].id, left);
    assert_eq!(children[0].style.opacity, 1.0);
    assert!((children[1].style.opacity - 0.5).abs() < 0.001);
    let right = children[1].id;
    click(
        &mut runtime,
        "blend-alpha",
        start + Duration::from_millis(150),
    );
    tick(&mut runtime, start + Duration::from_millis(250));
    let children = &node(&runtime, "crossfade").children;
    assert_eq!((children[0].id, children[1].id), (left, right));
    tick(&mut runtime, start + Duration::from_millis(500));
    let children = &node(&runtime, "crossfade").children;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, left);
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn a_failed_selection_discards_controls_and_the_next_selection_recovers() {
    let (_directory, mut runtime, start) = fixture();
    let before = runtime.root().unwrap().clone();
    let failed = node(&runtime, "fail").id;
    assert!(
        runtime
            .with_event_controls(Some(start), |runtime| {
                runtime.dispatch_checked_inner(failed, Event::Click, |_| Ok(()))
            })
            .is_err()
    );
    assert_eq!(runtime.root().unwrap(), &before);
    assert!(runtime.next_timer_deadline().is_none());
    assert!(!runtime.is_any_animating([before.id]));
    click(
        &mut runtime,
        "select-gamma",
        start + Duration::from_millis(20),
    );
    click(
        &mut runtime,
        "blend-alpha",
        start + Duration::from_millis(20),
    );
    tick(&mut runtime, start + Duration::from_millis(220));
    tick(&mut runtime, start + Duration::from_millis(420));
    assert_eq!(node(&runtime, "shown").text, "gamma");
    assert_eq!(node(&runtime, "phase").text, "idle");
    assert!(runtime.next_timer_deadline().is_none());
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn reloads_preserve_or_replace_the_sequence_and_retire_old_timer_tokens() {
    let (directory, mut runtime, start) = fixture();
    let entry = directory.path().join("panel.luau");
    click(&mut runtime, "select-beta", start);
    tick(&mut runtime, start + Duration::from_millis(60));
    let outgoing = node(&runtime, "replacement").children[0].id;
    fs::write(&entry, "error('bad edit')").unwrap();
    assert!(runtime.reload(&entry).is_err());
    tick(&mut runtime, start + Duration::from_millis(100));
    assert_eq!(node(&runtime, "replacement").children[0].id, outgoing);
    assert_eq!(node(&runtime, "requested").text, "beta");
    assert_eq!(node(&runtime, "phase").text, "closing");
    let old_token = runtime
        .due_timers(start + Duration::from_millis(200))
        .pop()
        .unwrap();
    fs::write(&entry, "return require('./gallery')(false, false)").unwrap();
    runtime.reload(&entry).unwrap();
    assert_eq!(node(&runtime, "shown").text, "alpha");
    assert_eq!(node(&runtime, "phase").text, "idle");
    assert!(runtime.next_timer_deadline().is_none());
    assert_eq!(
        runtime
            .dispatch_timer_checked(&old_token, start + Duration::from_millis(200), |_| Ok(()))
            .unwrap(),
        None
    );
}
