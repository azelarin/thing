use wayland_client::Proxy;

use super::*;
use crate::command::{CommandReply, CommandRequest, OutputSelection};

const SOURCE: &str = r#"
local destination = ui.state('old')
local count, opened = ui.state(0), ui.state('')
ui.command('replace', function(value) destination:set(value) end)
return ui.window {
    key = 'links',
    size = { width = 420, height = 120 },
    title = function() return `{count:get()}:{opened:get()}` end,
    children = { ui.text {
        key = 'label',
        spans = function() return {
            { text = 'first', link = destination:get() },
            ' and ',
            { text = 'second', link = 'second' },
        } end,
        on_link = function(target)
            count:set(count:get() + 1)
            opened:set(target)
        end,
        style = { width = 300, height = 80, font_family = 'DejaVu Sans', font_size = 20 },
    } },
}
"#;

fn start() -> (Host, u32, NodeId) {
    let mut host = Host::start(SOURCE, false);
    host.hold_callbacks(true);
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (420, 120), Vec::new());
    host.until(|host| host.frames(surface) > 0);
    let id = root(&host, surface).children[0].id;
    (host, surface, id)
}

fn surface_index(host: &Host, surface: u32) -> usize {
    host.app
        .surfaces
        .iter()
        .position(|candidate| candidate.role.surface().id().protocol_id() == surface)
        .unwrap()
}

fn root(host: &Host, surface: u32) -> &Node {
    let owner = &host.app.surfaces[surface_index(host, surface)];
    host.app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap()
}

fn point(host: &Host, surface: u32, id: NodeId, link: u16) -> Point {
    let layout = host.app.surfaces[surface_index(host, surface)]
        .layouts
        .presented()
        .unwrap();
    let rect = layout
        .link_regions(id)
        .iter()
        .find(|(index, _)| *index == link)
        .unwrap()
        .1;
    layout
        .get(id)
        .unwrap()
        .transform
        .point(Point {
            x: rect.x + rect.width / 2.0,
            y: rect.y + rect.height / 2.0,
        })
        .unwrap()
}

fn button(host: &mut Host, surface: u32, id: NodeId, link: u16, pressed: bool) {
    let point = point(host, surface, id, link);
    host.command(Command::Button {
        surface,
        x: f64::from(point.x),
        y: f64::from(point.y),
        pressed,
    });
    host.until(|host| {
        host.app.surfaces[surface_index(host, surface)]
            .input
            .pressed
            == if pressed { Some(id) } else { None }
    });
}

fn click(host: &mut Host, surface: u32, id: NodeId, link: u16) {
    button(host, surface, id, link, true);
    button(host, surface, id, link, false);
}

fn replace(host: &mut Host, destination: &str) {
    let reply = host.app.call_command(CommandRequest {
        command: "replace".into(),
        arguments: thing_runtime::CallValue::String(destination.into()),
        selection: OutputSelection::Implicit,
    });
    assert!(
        matches!(reply, CommandReply::Ran(results) if results.len() == 1 && results[0].result.is_ok())
    );
}

#[test]
fn pointer_links_do_not_activate_an_unpresented_destination() {
    let (mut host, surface, id) = start();
    let frames = host.frames(surface);
    replace(&mut host, "new");
    assert_eq!(&*root(&host, surface).children[0].links, ["new", "second"]);
    click(&mut host, surface, id, 0);
    assert_eq!(host.frames(surface), frames);
    assert_eq!(root(&host, surface).window.title, "0:");

    // an unchanged destination in the same widget still works.
    click(&mut host, surface, id, 1);
    assert_eq!(root(&host, surface).window.title, "1:second");

    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| host.frames(surface) > frames);
    click(&mut host, surface, id, 0);
    assert_eq!(root(&host, surface).window.title, "2:new");
}

fn key(host: &mut Host, keysym: Keysym) {
    host.app.key(
        KeyEvent {
            time: 1,
            raw_code: 28,
            keysym,
            utf8: None,
        },
        100,
        true,
        &host.qh,
    );
}

#[test]
fn keyboard_links_do_not_activate_an_unpresented_destination() {
    let (mut host, surface, id) = start();
    let index = surface_index(&host, surface);
    let output = host.app.surfaces[index].output.clone();
    let wl = host.app.surfaces[index].role.surface().clone();
    host.app.set_keyboard_surface(Some(wl));
    host.app.surfaces[index].focus_target = host.app.scoped_focus_target(&output, id);
    host.app.surfaces[index].input.focus_from_keyboard(id);
    replace(&mut host, "new");
    key(&mut host, Keysym::Return);
    assert_eq!(root(&host, surface).window.title, "0:");
    key(&mut host, Keysym::Right);
    assert_eq!(host.app.surfaces[index].input.focused_link, Some(1));
    key(&mut host, Keysym::Return);
    assert_eq!(root(&host, surface).window.title, "1:second");
}

#[test]
fn keyboard_selection_reveals_links_inside_a_scroll_view() {
    let mut host = Host::start(
        r#"
        return ui.window { key = 'links', size = { width = 420, height = 120 }, children = {
            ui.scroll { style = { width = 200, height = 40 }, children = {
                ui.text {
                    spans = { { text = 'first\n', link = 'one' }, { text = 'second\n', link = 'two' }, { text = 'third', link = 'three' } },
                    on_link = function() end,
                    style = { width = 200, height = 160, font_family = 'DejaVu Sans', font_size = 20 },
                },
            } },
        } }
    "#,
        false,
    );
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (420, 120), Vec::new());
    host.until(|host| host.frames(surface) > 0);
    let index = surface_index(&host, surface);
    let viewport = root(&host, surface).children[0].id;
    let id = root(&host, surface).children[0].children[0].id;
    let output = host.app.surfaces[index].output.clone();
    let wl = host.app.surfaces[index].role.surface().clone();
    host.app.set_keyboard_surface(Some(wl));
    host.app.surfaces[index].focus_target = host.app.scoped_focus_target(&output, id);
    host.app.surfaces[index].input.focus_from_keyboard(id);
    let frames = host.frames(surface);
    key(&mut host, Keysym::Right);
    host.until(|host| {
        host.frames(surface) > frames
            && host.app.surfaces[index]
                .layouts
                .presented()
                .and_then(|layout| layout.semantic_placement(viewport))
                .and_then(|placement| placement.scroll_offset)
                .is_some_and(|offset| offset > 0.0)
    });
    assert_eq!(host.app.surfaces[index].input.focused_link, Some(1));
    let layout = host.app.surfaces[index].layouts.presented().unwrap();
    let rect = layout
        .link_regions(id)
        .iter()
        .find(|(link, _)| *link == 1)
        .unwrap()
        .1;
    let placement = layout.get(id).unwrap();
    let visible = placement.transform.rect_aabb(rect).unwrap();
    assert!(visible.y >= placement.clip.y - 0.01, "{visible:?}");
    assert!(
        visible.y + visible.height <= placement.clip.y + placement.clip.height + 0.01,
        "{visible:?}"
    );
}

#[test]
fn pointer_links_keep_the_destination_that_was_pressed() {
    let (mut host, surface, id) = start();
    let frames = host.frames(surface);
    button(&mut host, surface, id, 0, true);
    replace(&mut host, "new");
    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| host.frames(surface) > frames);
    button(&mut host, surface, id, 0, false);
    assert_eq!(root(&host, surface).window.title, "0:");
    click(&mut host, surface, id, 0);
    assert_eq!(root(&host, surface).window.title, "1:new");
}

#[test]
fn dragging_between_distinct_links_does_not_activate_either() {
    let (mut host, surface, id) = start();
    button(&mut host, surface, id, 0, true);
    button(&mut host, surface, id, 1, false);
    assert_eq!(root(&host, surface).window.title, "0:");
    click(&mut host, surface, id, 1);
    assert_eq!(root(&host, surface).window.title, "1:second");
}
