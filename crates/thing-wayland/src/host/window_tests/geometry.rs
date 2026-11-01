//! `ui.geometry` and `ui.convert` through the real host and the private
//! compositor: measurements follow presented frames only, popups relate to
//! their parent through the position the compositor configured, and
//! feedback stays bounded across surfaces.

use thing_runtime::MAX_FEEDBACK_ROUNDS;
use wayland_client::Proxy;

use super::*;

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

/// the center of the widget keyed `key` in the frame `surface` presented.
fn center(host: &Host, surface: u32, key: &str) -> (f64, f64) {
    let surface = &host.app.surfaces[surface_index(host, surface)];
    let root = host
        .app
        .configurations
        .surface_root(&surface.output, &surface.key)
        .unwrap();
    let id = keyed(root, key).unwrap().id;
    let bounds = surface
        .layouts
        .presented()
        .and_then(|layout| layout.semantic_placement(id))
        .unwrap_or_else(|| panic!("{key} was not presented"))
        .bounds;
    (
        f64::from(bounds.x + bounds.width / 2.0),
        f64::from(bounds.y + bounds.height / 2.0),
    )
}

fn click(host: &mut Host, surface: u32, key: &str) {
    let (x, y) = center(host, surface, key);
    host.command(Command::Click { surface, x, y });
}

fn popup_position(host: &Host, surface: u32) -> Option<(i32, i32)> {
    match &host.app.surfaces[surface_index(host, surface)].role {
        Role::Popup(role) => role.position,
        _ => None,
    }
}

fn toplevel_titled(host: &Host, title: &str) -> (u32, u32) {
    host.toplevels()
        .into_iter()
        .find(|(toplevel, _)| {
            host.records()
                .contains(&Record::Title(*toplevel, title.into()))
        })
        .unwrap_or_else(|| panic!("no window titled {title}"))
}

/// turns the host a while, so a refresh or redraw loop would show up.
fn settle(host: &mut Host) {
    for _ in 0..20 {
        host.turn();
    }
}

/// a window and a second window whose title reports the first one's
/// measured root box and how often the title was evaluated.
const MONITOR: &str = r#"
local evaluations = 0
local measured
local main = ui.window {
    key = 'main',
    title = 'main',
    size = { width = 400, height = 300 },
    children = { ui.row { style = { width = 10, height = 10 } } },
}
measured = ui.geometry(main)
local monitor = ui.window {
    key = 'monitor',
    title = function()
        evaluations += 1
        local strict = services.clock.time:get() == 'strict'
        local layout = measured:layout()
        if strict and layout and layout.width > 800 then
            error('too wide')
        end
        return string.format('%d %s', evaluations,
            if layout then string.format('%gx%g', layout.width, layout.height) else 'none')
    end,
    size = { width = 200, height = 100 },
}
return { main, monitor }
"#;

#[test]
fn window_geometry_follows_presented_frames_until_the_window_closes() {
    let mut host = Host::start(MONITOR, false);
    host.until(|host| host.toplevels().len() == 2);
    let (main, surface) = toplevel_titled(&host, "main");
    let monitor = host
        .toplevels()
        .into_iter()
        .find(|(toplevel, _)| *toplevel != main)
        .unwrap()
        .0;
    let title = |host: &Host| host.title(monitor).unwrap_or_default();
    let base: u32 = title(&host)
        .strip_suffix(" none")
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("unexpected title {:?}", title(&host)));
    let expect = |offset: u32, size: &str| format!("{} {size}", base + offset);

    // nothing is measured until the window's first frame presents.
    host.configure(main, (0, 0), vec![]);
    host.until(|host| title(host) == expect(1, "400x300"));

    // an identical frame publishes nothing, so the title is not evaluated.
    let frames = host.frames(surface);
    host.configure(main, (400, 300), vec![]);
    host.until(|host| host.frames(surface) > frames);
    settle(&mut host);
    assert_eq!(title(&host), expect(1, "400x300"));

    // geometry is logical, so a new output scale changes nothing either.
    let owner = SurfaceOwner {
        output: host.app.configurations.keys().next().unwrap().clone(),
        key: SurfaceKey::Named("main".into()),
    };
    let frames = host.frames(surface);
    host.command(Command::PreferredScale { surface, scale: 2 });
    host.configure(main, (400, 300), vec![]);
    host.until(|host| {
        host.frames(surface) > frames
            && host.app.surfaces[host.app.root_index(&owner).unwrap()]
                .scale
                .factor()
                == 2.0
    });
    settle(&mut host);
    assert_eq!(title(&host), expect(1, "400x300"));

    // a frame whose callback the compositor keeps still publishes once.
    host.hold_callbacks(true);
    host.configure(main, (500, 300), vec![]);
    host.until(|host| title(host) == expect(2, "500x300"));
    // the next configure is acknowledged, but its frame waits for the
    // callback, so the old geometry stays and nothing spins meanwhile.
    let configured = |host: &Host| {
        host.records()
            .iter()
            .filter_map(|record| match record {
                Record::Configure {
                    surface: configured,
                    serial,
                } if *configured == surface => Some(*serial),
                _ => None,
            })
            .max()
    };
    let frames = host.frames(surface);
    let titles = host.records().len();
    let before = configured(&host);
    host.configure(main, (600, 300), vec![]);
    host.until(|host| configured(host) > before);
    let serial = configured(&host).unwrap();
    host.until(|host| host.has(&Record::Ack { surface, serial }));
    settle(&mut host);
    assert_eq!(host.frames(surface), frames);
    assert_eq!(title(&host), expect(2, "500x300"));
    assert!(
        host.records()[titles..]
            .iter()
            .all(|record| !matches!(record, Record::Title(id, _) if *id == monitor))
    );
    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| title(host) == expect(3, "600x300"));

    // a dependent that rejects the new geometry keeps the accepted title
    // and does not redraw; the next valid frame recovers.
    host.set_service("clock.time", "strict");
    host.until(|host| title(host) == expect(4, "600x300"));
    let frames = host.frames(surface);
    host.configure(main, (900, 300), vec![]);
    host.until(|host| host.frames(surface) > frames);
    settle(&mut host);
    assert_eq!(host.frames(surface), frames + 1);
    assert_eq!(title(&host), expect(4, "600x300"));
    host.configure(main, (700, 300), vec![]);
    host.until(|host| title(host).ends_with(" 700x300"));

    // a closed window may stay in the tree, but its geometry goes with it.
    host.command(Command::Close(main));
    host.until(|host| host.has(&Record::DestroyToplevel(main)));
    host.until(|host| title(host).ends_with(" none"));
}

/// a window with a popup. the window title reports how often the probe ran
/// and its last conversions, the popup widget's measured position, and
/// whether a popup nested inside it was ever measured.
const POPUP: &str = r#"
local open = ui.state(false)
local shift = ui.state(0)
local result = ui.state('')
local probes = 0
local main
local inner = ui.row { style = { width = 10, height = 10 } }
local deep = ui.row { style = { width = 4, height = 4 } }
local measured = ui.geometry(inner)
local nested = ui.geometry(deep)
local function point(value)
    return if value then string.format('%g,%g', value.x, value.y) else 'nil'
end
main = ui.window {
    key = 'main',
    title = function()
        local layout = measured:layout()
        return string.format('%s|%s|%s', result:get(),
            if layout then string.format('%g,%g', layout.x, layout.y) else 'none',
            tostring(nested:available()))
    end,
    size = { width = 400, height = 300 },
    children = {
        ui.row { style = { width = function() return 10 + shift:get() end, height = 10 } },
        ui.button {
            key = 'opener',
            style = { width = 100, height = 40 },
            on_click = function() open:set(not open:get()) end,
            children = { ui.popup {
                key = 'menu',
                visible = function() return open:get() end,
                style = { width = 100, height = 60 },
                children = { ui.button {
                    key = 'probe',
                    style = { width = 100, height = 60 },
                    on_click = function()
                        probes += 1
                        local outward = ui.convert(inner, 0, 0, main)
                        local back = outward and ui.convert(main, outward.x, outward.y, inner)
                        result:set(string.format('%d %s %s %s', probes, point(outward),
                            point(back), point(ui.convert(deep, 0, 0, main))))
                    end,
                    children = { inner, ui.popup { children = { deep } } },
                } },
            } },
        },
        ui.button {
            key = 'mover',
            style = { width = 20, height = 40 },
            on_click = function() shift:set(shift:get() + 30) end,
        },
    },
}
return { main }
"#;

#[test]
fn popups_relate_to_their_window_through_the_configured_origin_once_presented() {
    let mut host = Host::start(POPUP, false);
    let (main, surface) = host.toplevels()[0];
    let title = |host: &Host| host.title(main).unwrap_or_default();
    host.configure(main, (0, 0), vec![]);
    host.until(|host| host.frames(surface) > 0);
    assert_eq!(title(&host), "|none|false");

    host.set_popup_position((30, 70));
    click(&mut host, surface, "opener");
    host.until(|host| !host.popups().is_empty());
    let popup = host.popups()[0];
    host.until(|host| !title(host).contains("|none|"));
    // the popup widget's position is in the popup's own coordinates.
    let local = title(&host);
    let local = local.trim_start_matches('|');
    let (x, y) = local
        .split_once('|')
        .and_then(|(point, _)| point.split_once(','))
        .map(|(x, y)| (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap()))
        .unwrap();
    let probed = |host: &Host, count: u32, origin: (f64, f64)| {
        title(host)
            == format!(
                "{count} {},{} 0,0 nil|{x},{y}|false",
                origin.0 + x,
                origin.1 + y
            )
    };

    // conversion adds the origin on the way out and removes it on the way
    // back. the nested popup never opens, so it has no position.
    click(&mut host, popup, "probe");
    host.until(|host| probed(host, 1, (30.0, 70.0)));

    // a compositor move applies with the popup's next presented frame. the
    // first configure here draws a frame whose callback stays pending.
    host.hold_callbacks(true);
    let frames = host.frames(popup);
    host.command(Command::ConfigurePopup {
        surface: popup,
        position: (30, 70),
    });
    host.until(|host| host.frames(popup) > frames);
    host.command(Command::ConfigurePopup {
        surface: popup,
        position: (45, 90),
    });
    host.until(|host| popup_position(host, popup) == Some((45, 90)));
    settle(&mut host);
    assert_eq!(host.frames(popup), frames + 1);
    // until the popup presents again, conversion uses the frame on screen.
    click(&mut host, popup, "probe");
    host.until(|host| title(host).starts_with("2 "));
    assert!(probed(&host, 2, (30.0, 70.0)), "{}", title(&host));
    host.hold_callbacks(false);
    host.command(Command::ReleaseCallbacks);
    host.until(|host| host.frames(popup) > frames + 1);
    click(&mut host, popup, "probe");
    host.until(|host| probed(host, 3, (45.0, 90.0)));

    // the host asks to move the popup when its anchor moves; the reply
    // places it at the new position.
    host.set_popup_position((60, 20));
    click(&mut host, surface, "mover");
    host.until(|host| {
        host.records()
            .iter()
            .any(|record| matches!(record, Record::Reposition { surface, .. } if *surface == popup))
    });
    host.until(|host| popup_position(host, popup) == Some((60, 20)));
    let frames = host.frames(popup);
    host.until(|host| host.frames(popup) > frames);
    click(&mut host, popup, "probe");
    host.until(|host| probed(host, 4, (60.0, 20.0)));

    // closing the popup takes its measurements with it.
    click(&mut host, surface, "opener");
    host.until(|host| host.has(&Record::DestroyPopup(popup)));
    host.until(|host| title(host).ends_with("|none|false"));
}

/// a panel box as wide as the popup box plus one, and a popup box as wide
/// as the panel box plus one, so each surface keeps changing the other.
const CROSSING: &str = r#"
local open = ui.state(false)
local panel_geometry, popup_geometry
local panel_box = ui.row { key = 'panel_box', style = { width = function()
    local layout = popup_geometry and popup_geometry:layout()
    return if layout then layout.width + 1 else 20
end, height = 10 } }
local popup_box = ui.row { key = 'popup_box', style = { width = function()
    local layout = panel_geometry and panel_geometry:layout()
    return if layout then layout.width + 1 else 20
end, height = 10 } }
panel_geometry = ui.geometry(panel_box)
popup_geometry = ui.geometry(popup_box)
return {
    ui.panel {
        key = 'bar',
        style = { height = 40 },
        children = {
            panel_box,
            ui.button {
                key = 'opener',
                style = { width = 100, height = 30 },
                on_click = function() open:set(true) end,
                children = { ui.popup {
                    visible = function() return open:get() end,
                    style = { width = 100, height = 60 },
                    children = { popup_box },
                } },
            },
        },
    },
    -- the fixture starts once a toplevel exists; this one stays unconfigured.
    ui.window { key = 'idle', title = 'idle', size = { width = 100, height = 100 } },
}
"#;

#[test]
fn feedback_between_a_panel_and_its_popup_stops_on_both_surfaces() {
    let mut host = Host::start(CROSSING, true);
    host.until(|host| {
        host.records()
            .iter()
            .any(|record| matches!(record, Record::Layer(_)))
    });
    let panel = host
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::Layer(surface) => Some(surface),
            _ => None,
        })
        .unwrap();
    host.until(|host| host.frames(panel) > 0);
    click(&mut host, panel, "opener");
    host.until(|host| !host.popups().is_empty());
    let popup = host.popups()[0];
    host.until(|host| host.frames(popup) > 0);

    // each surface's chain sees only geometry's own changes, so both stop
    // after a bounded number of rounds instead of feeding each other.
    let total = |host: &Host| host.frames(panel) + host.frames(popup);
    let describe = |host: &Host| {
        let records = host.records();
        let count = |pattern: fn(&Record) -> bool| records.iter().filter(|r| pattern(r)).count();
        let owner = &host.app.surfaces[surface_index(host, panel)];
        let root = host
            .app
            .configurations
            .surface_root(&owner.output, &owner.key)
            .unwrap();
        format!(
            "panel {} popup {} popups {} destroyed {} repositions {} widths {:?} {:?}",
            host.frames(panel),
            host.frames(popup),
            count(|r| matches!(r, Record::Popup { .. })),
            count(|r| matches!(r, Record::DestroyPopup(_))),
            count(|r| matches!(r, Record::Reposition { .. })),
            keyed(root, "panel_box").and_then(|node| node.style.width),
            keyed(root, "popup_box").and_then(|node| node.style.width),
        )
    };
    let mut previous = total(&host);
    let mut quiet = 0;
    let deadline = Instant::now() + WAIT;
    while quiet < 20 {
        assert!(
            Instant::now() < deadline,
            "frames kept coming: {}",
            describe(&host)
        );
        host.turn();
        if total(&host) == previous {
            quiet += 1;
        } else {
            previous = total(&host);
            quiet = 0;
        }
    }
    // each publication can redraw both surfaces, including one unchanged
    // frame. include initial configuration and the opening interaction.
    let bound = 4 * (MAX_FEEDBACK_ROUNDS as usize + 3);
    assert!(previous <= bound, "{previous} frames");
    let owner = &host.app.surfaces[surface_index(&host, panel)];
    let root = host
        .app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap();
    for key in ["panel_box", "popup_box"] {
        let width = keyed(root, key).unwrap().style.width.unwrap();
        assert!(
            width <= 20.0 + 2.0 * (MAX_FEEDBACK_ROUNDS + 3) as f32,
            "{key}: {width}"
        );
    }
}
