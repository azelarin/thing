//! live inspection through the real host and the private compositor: reads
//! and highlights leave ordinary input and focus alone, and highlights bind
//! only to identities that a presented frame showed.

use serde_json::{Value as Json, json};
use wayland_client::Proxy;

use super::*;
use crate::{
    command::{CommandReply, CommandRequest, OutputSelection},
    host::inspection::Inspector,
    inspection::{InspectQuery, InspectReply, InspectRequest},
};

/// a window whose title counts clicks. `list` rebuilds an unkeyed and a keyed
/// button on every `replace`, `locked` is a disabled button that can be hidden
/// or removed, and the opener shows a popup.
const SOURCE: &str = r#"
local clicks = ui.state(0)
local generation = ui.state(0)
local hidden = ui.state(false)
local removed = ui.state(false)
local open = ui.state(false)
ui.command('replace', function() generation:set(generation:get() + 1) end)
ui.command('hide', function() hidden:set(true) end)
ui.command('show', function() hidden:set(false) end)
ui.command('remove', function() removed:set(true) end)
ui.command('close', function() open:set(false) end)
return ui.window {
    key = 'inspect',
    title = function() return `clicks {clicks:get()}` end,
    size = { width = 400, height = 300 },
    children = { ui.column { children = {
        ui.button {
            key = 'clicker',
            style = { width = 100, height = 40 },
            on_click = function() clicks:set(clicks:get() + 1) end,
        },
        -- popups need the serial of the click that opens them.
        ui.button {
            key = 'opener',
            style = { width = 100, height = 40 },
            on_click = function() open:set(true) end,
            children = { ui.popup {
                key = 'menu',
                visible = function() return open:get() end,
                style = { width = 100, height = 60 },
                children = { ui.row { key = 'item', style = { width = 80, height = 30 } } },
            } },
        },
        ui.row { key = 'list', children = function()
            local width = 40 + generation:get()
            return {
                ui.button { style = { width = width, height = 30 }, on_click = function() end },
                ui.button {
                    key = 'keyed',
                    style = { width = width, height = 30 },
                    on_click = function() end,
                },
            }
        end },
        ui.row { key = 'holder', children = function()
            if removed:get() then return {} end
            return { ui.button {
                key = 'locked',
                enabled = false,
                visible = function() return not hidden:get() end,
                style = { width = 60, height = 30 },
                on_click = function() end,
            } }
        end },
    } } },
}
"#;

/// starts an inspecting host and waits for the window's first frame. with
/// `hold`, that frame's callback stays pending, so nothing else presents.
fn start(hold: bool) -> (Host, u32, u32) {
    let mut host = Host::start(SOURCE, false);
    host.app.inspector = Some(Inspector::default());
    host.hold_callbacks(hold);
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (400, 300), Vec::new());
    host.until(|host| {
        host.frames(surface) > 0
            && host.app.surfaces[surface_index(host, surface)]
                .inspection
                .is_some()
    });
    (host, toplevel, surface)
}

fn surface_index(host: &Host, surface: u32) -> usize {
    host.app
        .surfaces
        .iter()
        .position(|candidate| candidate.role.surface().id().protocol_id() == surface)
        .unwrap_or_else(|| panic!("no host surface {surface}"))
}

fn keyed<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    if node.key.as_deref() == Some(key) {
        return Some(node);
    }
    node.children.iter().find_map(|child| keyed(child, key))
}

fn window_root(host: &Host) -> &Node {
    let owner = &host.app.surfaces[0];
    host.app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap()
}

fn id(host: &Host, key: &str) -> NodeId {
    keyed(window_root(host), key)
        .unwrap_or_else(|| panic!("no widget keyed {key}"))
        .id
}

fn list(host: &Host) -> (NodeId, NodeId) {
    let list = keyed(window_root(host), "list").unwrap();
    (list.children[0].id, list.children[1].id)
}

/// the bounds of `id` in the frame `surface` last presented to inspection.
fn snapshot_bounds(host: &Host, surface: u32, id: NodeId) -> Option<Rect> {
    host.app.surfaces[surface_index(host, surface)]
        .inspection
        .as_ref()?
        .layout
        .semantic_placement(id)
        .map(|placement| placement.bounds)
}

/// the center of `id` in the window's presented frame.
fn center(host: &Host, surface: u32, id: NodeId) -> (f64, f64) {
    let bounds = host.app.surfaces[surface_index(host, surface)]
        .layouts
        .presented()
        .and_then(|layout| layout.semantic_placement(id))
        .unwrap()
        .bounds;
    (
        f64::from(bounds.x + bounds.width / 2.0),
        f64::from(bounds.y + bounds.height / 2.0),
    )
}

fn command(host: &mut Host, name: &str) {
    let reply = host.app.call_command(CommandRequest {
        command: name.into(),
        arguments: thing_runtime::CallValue::Nil,
        selection: OutputSelection::Implicit,
    });
    assert!(
        matches!(&reply, CommandReply::Ran(results) if results.iter().all(|result| result.result.is_ok())),
        "{reply:?}"
    );
}

fn inspect(host: &mut Host, query: InspectQuery) -> Result<Json, String> {
    match host.app.inspect_request(InspectRequest {
        selection: OutputSelection::Implicit,
        query,
    }) {
        InspectReply::Failed(error) => Err(error),
        InspectReply::Reported(mut reports) => {
            assert_eq!(reports.len(), 1);
            reports.remove(0).result
        }
    }
}

fn node_query(id: NodeId, highlight: bool) -> InspectQuery {
    InspectQuery {
        node: Some(id.0),
        highlight: highlight.then_some(Duration::from_secs(30)),
        ..InspectQuery::default()
    }
}

fn highlighted(host: &Host) -> bool {
    host.app
        .inspector
        .as_ref()
        .is_some_and(|inspector| inspector.deadline().is_some())
}

/// pointer input, window focus, and keyboard focus, as one comparable value.
fn focus_state(host: &Host, surface: u32) -> String {
    let state = &host.app.surfaces[surface_index(host, surface)];
    let node = |target: Option<&ScopedUiTarget>| target.map(|target| target.target.node());
    format!(
        "{:?} {:?} {:?} {:?} {:?}",
        state.input,
        node(state.focus_target.as_ref()),
        node(state.pressed_target.as_ref()),
        host.app.keyboard_surface.as_ref().map(|wl| wl.id()),
        host.app.pointer_capture.is_some(),
    )
}

#[test]
fn inspecting_at_the_pointer_leaves_clicks_and_focus_alone() {
    let (mut host, toplevel, surface) = start(false);
    let index = surface_index(&host, surface);
    let wl = host.app.surfaces[index].role.surface().clone();
    host.app.set_keyboard_surface(Some(wl.clone()));
    let clicker = id(&host, "clicker");
    let (x, y) = center(&host, surface, clicker);
    assert_eq!(host.title(toplevel).as_deref(), Some("clicks 0"));

    // a press focuses the button; inspection between press and release
    // must not cancel the click or move focus.
    host.command(Command::Button {
        surface,
        x,
        y,
        pressed: true,
    });
    host.until(|host| host.app.surfaces[index].input.pressed == Some(clicker));
    assert_eq!(host.app.surfaces[index].input.focused, Some(clicker));
    let before = focus_state(&host, surface);
    let report = inspect(
        &mut host,
        InspectQuery {
            at_pointer: true,
            highlight: Some(Duration::from_secs(30)),
            ..InspectQuery::default()
        },
    )
    .unwrap();
    assert_eq!(report["surfaces"][0]["picked"], json!(clicker.0));
    assert!(highlighted(&host));
    inspect(&mut host, InspectQuery::default()).unwrap();
    assert_eq!(focus_state(&host, surface), before);
    assert_eq!(host.app.keyboard_surface.as_ref(), Some(&wl));

    host.command(Command::Button {
        surface,
        x,
        y,
        pressed: false,
    });
    host.until(|host| host.title(toplevel).as_deref() == Some("clicks 1"));
    assert_eq!(host.app.surfaces[index].input.focused, Some(clicker));

    // a whole click after inspection also reaches the handler.
    host.command(Command::Click { surface, x, y });
    host.until(|host| host.title(toplevel).as_deref() == Some("clicks 2"));
}

#[test]
fn highlights_bind_only_to_identities_the_presented_frame_showed() {
    let (mut host, _, surface) = start(true);
    let (unkeyed, keyed) = list(&host);
    let frames = host.frames(surface);
    command(&mut host, "replace");
    assert_eq!(
        list(&host),
        (unkeyed, keyed),
        "the replacements reuse the presented node ids"
    );
    assert_eq!(host.frames(surface), frames);

    // the frame on screen shows the old unkeyed button. its reused id now
    // names a different widget, which must not be highlighted.
    let _ = inspect(&mut host, node_query(unkeyed, true));
    assert!(
        !highlighted(&host),
        "an unpresented replacement was highlighted"
    );

    // a keyed widget keeps its identity through recreation.
    inspect(&mut host, node_query(keyed, true)).unwrap();
    assert!(highlighted(&host));

    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| {
        snapshot_bounds(host, surface, unkeyed).is_some_and(|bounds| bounds.width == 41.0)
    });
    // the keyed highlight survives the frame that shows its recreation.
    assert!(highlighted(&host));

    // once the replacement presents, the same id highlights it.
    inspect(&mut host, node_query(unkeyed, true)).unwrap();
    assert!(highlighted(&host));
}

#[test]
fn disabled_widgets_highlight_until_hidden_or_removed() {
    let (mut host, _, surface) = start(false);
    let locked = id(&host, "locked");
    let report = inspect(&mut host, node_query(locked, true)).unwrap();
    let node = &report["surfaces"][0]["nodes"][0];
    assert_eq!(node["id"], json!(locked.0));
    assert_eq!(node["enabled"], json!(false));
    assert!(node["bounds"].is_array(), "{node}");
    assert!(highlighted(&host));

    command(&mut host, "hide");
    host.until(|host| !highlighted(host));

    command(&mut host, "show");
    host.until(|host| snapshot_bounds(host, surface, locked).is_some());
    inspect(&mut host, node_query(locked, true)).unwrap();
    assert!(highlighted(&host));

    command(&mut host, "remove");
    host.until(|host| !highlighted(host));
}

#[test]
fn popup_reports_keep_the_presented_origin_and_closing_ends_the_highlight() {
    let (mut host, _, surface) = start(false);
    host.set_popup_position((30, 70));
    let (x, y) = center(&host, surface, id(&host, "opener"));
    host.command(Command::Click { surface, x, y });
    host.until(|host| !host.popups().is_empty());
    let popup = host.popups()[0];
    host.until(|host| {
        host.frames(popup) > 0
            && host.app.surfaces[surface_index(host, popup)]
                .inspection
                .is_some()
    });
    let item = id(&host, "item");
    let menu = id(&host, "menu");
    let section = |report: &Json| report["surfaces"][0].clone();
    let report = inspect(&mut host, node_query(item, true)).unwrap();
    assert_eq!(section(&report)["role"], "popup");
    assert_eq!(section(&report)["root"], json!(menu.0));
    assert_eq!(section(&report)["origin"], json!([30.0, 70.0]));
    assert!(highlighted(&host));

    // the first configure draws a frame whose callback stays pending, so the
    // second one is applied but not presented.
    host.hold_callbacks(true);
    let frames = host.frames(popup);
    host.command(Command::ConfigurePopup {
        surface: popup,
        position: (30, 70),
    });
    host.until(|host| host.frames(popup) > frames);
    let frames = host.frames(popup);
    host.command(Command::ConfigurePopup {
        surface: popup,
        position: (45, 90),
    });
    host.until(|host| {
        matches!(
            &host.app.surfaces[surface_index(host, popup)].role,
            Role::Popup(role) if role.position == Some((45, 90))
        )
    });
    assert_eq!(host.frames(popup), frames);
    let report = inspect(&mut host, node_query(item, false)).unwrap();
    assert_eq!(section(&report)["origin"], json!([30.0, 70.0]));

    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| {
        host.app.surfaces[surface_index(host, popup)]
            .inspection
            .as_ref()
            .is_some_and(|snapshot| snapshot.origin == Some((45.0, 90.0)))
    });
    let report = inspect(&mut host, node_query(item, false)).unwrap();
    assert_eq!(section(&report)["origin"], json!([45.0, 90.0]));
    assert!(highlighted(&host));

    command(&mut host, "close");
    host.until(|host| host.has(&Record::DestroyPopup(popup)));
    host.until(|host| !highlighted(host));
}
