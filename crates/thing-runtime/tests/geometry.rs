use std::{fs, path::PathBuf, rc::Rc};

use tempfile::TempDir;
use thing_core::{Node, NodeId};
use thing_runtime::{
    BoundsProvider, BoundsRect, Event, GeometryUpdate, MAX_FEEDBACK_ROUNDS, MappedPoint,
    PresentedBounds, Runtime, WidgetBounds,
};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

fn load(source: &str) -> (TempDir, Runtime) {
    let (directory, path) = fixture(source);
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    if node.key.as_deref() == Some(key) {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, key))
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    runtime
        .surfaces()
        .iter()
        .find_map(|surface| find(&surface.root, key))
        .unwrap_or_else(|| panic!("no widget keyed {key}"))
}

fn text(runtime: &Runtime, key: &str) -> String {
    node(runtime, key).text.clone()
}

fn click(runtime: &mut Runtime, key: &str) {
    let id = node(runtime, key).id;
    runtime.dispatch(id, Event::Click).unwrap();
}

/// one widget in a fake frame: its layout box and a uniform scale about the
/// box's origin.
#[derive(Clone, Copy)]
struct Placed {
    node: NodeId,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    content: Option<(f64, f64)>,
}

fn placed(node: NodeId, x: f64, y: f64, width: f64, height: f64) -> Placed {
    Placed {
        node,
        x,
        y,
        width,
        height,
        scale: 1.0,
        content: None,
    }
}

#[derive(Default)]
struct Frame {
    widgets: Vec<Placed>,
    origin: Option<(f64, f64)>,
    /// the host resized or scrolled the surface since its previous frame.
    moved: bool,
}

impl Frame {
    fn with(mut self, widget: Placed) -> Self {
        self.widgets.push(widget);
        self
    }

    fn find(&self, node: NodeId) -> Option<Placed> {
        self.widgets
            .iter()
            .find(|widget| widget.node == node)
            .copied()
    }
}

impl BoundsProvider for Frame {
    fn get(&self, node: NodeId) -> Option<WidgetBounds> {
        let widget = self.find(node)?;
        Some(WidgetBounds {
            node,
            layout: BoundsRect {
                x: widget.x,
                y: widget.y,
                width: widget.width,
                height: widget.height,
            },
            visual: BoundsRect {
                x: widget.x,
                y: widget.y,
                width: widget.width * widget.scale,
                height: widget.height * widget.scale,
            },
        })
    }

    fn content(&self, node: NodeId) -> Option<(f64, f64)> {
        self.find(node)?.content
    }

    fn local_to_surface(&self, node: NodeId, x: f64, y: f64) -> MappedPoint {
        match self.find(node) {
            Some(widget) => MappedPoint::Point {
                x: widget.x + x * widget.scale,
                y: widget.y + y * widget.scale,
            },
            None => MappedPoint::Unavailable,
        }
    }

    fn surface_to_local(&self, node: NodeId, x: f64, y: f64) -> MappedPoint {
        match self.find(node) {
            Some(widget) if widget.scale == 0.0 => MappedPoint::Noninvertible,
            Some(widget) => MappedPoint::Point {
                x: (x - widget.x) / widget.scale,
                y: (y - widget.y) / widget.scale,
            },
            None => MappedPoint::Unavailable,
        }
    }

    fn origin(&self) -> Option<(f64, f64)> {
        self.origin
    }

    fn moved(&self) -> bool {
        self.moved
    }
}

/// what the host does after presenting: retain the frame, publish its
/// geometry, and refresh when that changed a handle.
fn present(runtime: &mut Runtime, frame: Frame) -> (Rc<PresentedBounds>, GeometryUpdate) {
    let root = runtime.surfaces()[0].root.id;
    present_at(runtime, root, frame)
}

fn present_at(
    runtime: &mut Runtime,
    root: NodeId,
    frame: Frame,
) -> (Rc<PresentedBounds>, GeometryUpdate) {
    let frame = Rc::new(frame);
    let lease = runtime
        .record_presented_bounds(root, frame.clone())
        .unwrap();
    let update = runtime.publish_geometry(root, frame.as_ref()).unwrap();
    if runtime.take_geometry_refresh() {
        runtime.refresh().unwrap();
    }
    (lease, update)
}

fn root(runtime: &Runtime) -> NodeId {
    runtime.root().unwrap().id
}

const LABEL: &str = r#"
    local shown = ui.state(true)
    local observed = ui.state(0)
    local label = ui.text { key = 'label', text = 'hello' }
    local geometry = ui.geometry(label)
    assert(ui.geometry(label) ~= nil)
    return ui.panel { children = {
        ui.row { key = 'holder', children = function()
            return if shown:get() then { label } else {}
        end },
        ui.button { key = 'toggle', on_click = function() shown:set(not shown:get()) end },
        ui.button { key = 'again', on_click = function()
            geometry = ui.geometry(label)
            observed:set(observed:get() + 1)
        end },
        ui.text { key = 'out', text = function()
            observed:get()
            if not geometry:available() then
                assert(geometry:layout() == nil and geometry:content() == nil)
                return 'none'
            end
            local layout, visual, content = geometry:layout(), geometry:visual(), geometry:content()
            assert(not pcall(function() layout.width = 0 end))
            return string.format('%g %g %g %s', layout.x, layout.width, visual.width,
                if content then string.format('%gx%g', content.width, content.height) else '-')
        end },
    } }
"#;

fn label_frame(runtime: &Runtime, width: f64) -> Frame {
    let label = node(runtime, "label").id;
    Frame::default()
        .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
        .with(Placed {
            content: Some((width, 20.0)),
            scale: 2.0,
            ..placed(label, 10.0, 5.0, width, 20.0)
        })
}

#[test]
fn handles_are_unavailable_until_a_frame_measures_them_and_publish_only_changes() {
    let (_directory, mut runtime) = load(LABEL);
    assert_eq!(text(&runtime, "out"), "none");
    // only observed widgets ask layout for an intrinsic measurement.
    assert!(node(&runtime, "label").measure_content);
    assert!(!node(&runtime, "out").measure_content);
    assert!(!node(&runtime, "holder").measure_content);

    let frame = label_frame(&runtime, 40.0);
    let (_lease, update) = present(&mut runtime, frame);
    assert!(update.changed);
    assert_eq!(text(&runtime, "out"), "10 40 80 40x20");

    // an identical frame publishes nothing and schedules no refresh.
    let frame = label_frame(&runtime, 40.0);
    let (_lease, update) = present(&mut runtime, frame);
    assert_eq!(update, GeometryUpdate::default());
    assert!(!runtime.take_geometry_refresh());

    let frame = label_frame(&runtime, 64.0);
    let (_lease, update) = present(&mut runtime, frame);
    assert!(update.changed);
    assert_eq!(text(&runtime, "out"), "10 64 128 64x20");

    // a frame without a placement, as for a hidden widget, is unavailable.
    let frame = Frame::default().with(placed(root(&runtime), 0.0, 0.0, 800.0, 30.0));
    let (_lease, update) = present(&mut runtime, frame);
    assert!(update.changed);
    assert_eq!(text(&runtime, "out"), "none");
}

#[test]
fn removed_widgets_retire_until_asked_for_again() {
    let (_directory, mut runtime) = load(LABEL);
    let frame = label_frame(&runtime, 40.0);
    let _lease = present(&mut runtime, frame);
    assert_eq!(text(&runtime, "out"), "10 40 80 40x20");
    let old = node(&runtime, "label").id;

    click(&mut runtime, "toggle");
    // removal clears the measurement without waiting for another frame.
    assert!(runtime.take_geometry_refresh());
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "none");

    click(&mut runtime, "toggle");
    let replacement = node(&runtime, "label").id;
    assert_ne!(old, replacement);
    // a retired handle neither measures nor reads the returning widget.
    assert!(!node(&runtime, "label").measure_content);
    let frame = label_frame(&runtime, 40.0);
    let (_lease, update) = present(&mut runtime, frame);
    assert!(!update.changed);
    assert_eq!(text(&runtime, "out"), "none");

    // asking for the same widget table again starts a fresh identity.
    click(&mut runtime, "again");
    assert!(node(&runtime, "label").measure_content);
    let frame = label_frame(&runtime, 40.0);
    let _lease = present(&mut runtime, frame);
    assert_eq!(text(&runtime, "out"), "10 40 80 40x20");
}

#[test]
fn observing_a_returning_widget_does_not_revive_old_handles_or_methods() {
    let (_directory, mut runtime) = load(
        r#"
        local shown = ui.state(true)
        local observed = ui.state(0)
        local label = ui.text { key = 'label', text = 'hello' }
        local original = ui.geometry(label)
        local old_method = original.layout
        local current = original
        local function describe(layout)
            return if layout then tostring(layout.width) else 'none'
        end
        return ui.panel { children = {
            ui.row { children = function() return if shown:get() then { label } else {} end },
            ui.button { key = 'toggle', on_click = function() shown:set(not shown:get()) end },
            ui.button { key = 'again', on_click = function()
                current = ui.geometry(label)
                observed:set(observed:get() + 1)
            end },
            ui.text { key = 'out', text = function()
                observed:get()
                return describe(original:layout()) .. ' ' .. describe(old_method()) .. ' ' .. describe(current:layout())
            end },
        } }
        "#,
    );
    let first = label_frame(&runtime, 40.0);
    let _lease = present(&mut runtime, first);
    assert_eq!(text(&runtime, "out"), "40 40 40");
    click(&mut runtime, "toggle");
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "none none none");
    click(&mut runtime, "toggle");
    click(&mut runtime, "again");
    let returned = label_frame(&runtime, 80.0);
    let _lease = present(&mut runtime, returned);
    assert_eq!(text(&runtime, "out"), "none none 80");
}

#[test]
fn keyed_lists_keep_handles_and_unrelated_replacements_never_inherit_them() {
    let (_directory, mut runtime) = load(
        r#"
        local items = ui.state('a,b')
        local plain = ui.state('first')
        local watched = ui.state(false)
        local widgets = {}
        local keyed, unkeyed
        local function describe(handle)
            if not handle or not handle:available() then return 'none' end
            return tostring(handle:layout().width)
        end
        return ui.panel { children = {
            ui.column { key = 'list', children = function()
                local out = {}
                for name in string.gmatch(items:get(), '[^,]+') do
                    widgets[name] = ui.text { key = name, text = name }
                    table.insert(out, widgets[name])
                end
                return out
            end },
            ui.column { key = 'plain', children = function()
                widgets.plain = ui.text { text = plain:get() }
                return { widgets.plain }
            end },
            ui.button { key = 'watch', on_click = function()
                keyed = ui.geometry(widgets.a)
                unkeyed = ui.geometry(widgets.plain)
                watched:set(true)
            end },
            ui.button { key = 'reorder', on_click = function() items:set('c,a,b') end },
            ui.button { key = 'drop', on_click = function() items:set('b') end },
            ui.button { key = 'restore', on_click = function() items:set('a,b') end },
            ui.button { key = 'replace', on_click = function() plain:set('second') end },
            ui.text { key = 'out', text = function()
                if not watched:get() then return '' end
                return describe(keyed) .. ' ' .. describe(unkeyed)
            end },
        } }
        "#,
    );
    let frame = |runtime: &Runtime, width: f64| {
        let plain = node(runtime, "plain").children[0].id;
        let mut frame = Frame::default().with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0));
        frame = frame.with(placed(plain, 0.0, 0.0, width + 1.0, 10.0));
        if let Some(a) = find(&runtime.surfaces()[0].root, "a") {
            frame = frame.with(placed(a.id, 0.0, 0.0, width, 10.0));
        }
        frame
    };
    click(&mut runtime, "watch");
    let first = frame(&runtime, 30.0);
    let _lease = present(&mut runtime, first);
    assert_eq!(text(&runtime, "out"), "30 31");
    let a = node(&runtime, "a").id;

    // keyed re-creation keeps the node and key while the widget table changes.
    click(&mut runtime, "reorder");
    assert_eq!(node(&runtime, "a").id, a);
    let reordered = frame(&runtime, 44.0);
    let _lease = present(&mut runtime, reordered);
    assert_eq!(text(&runtime, "out"), "44 45");

    // an unkeyed replacement reuses the numeric node id but not the identity.
    let plain = node(&runtime, "plain").children[0].id;
    click(&mut runtime, "replace");
    assert_eq!(node(&runtime, "plain").children[0].id, plain);
    runtime.take_geometry_refresh();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "44 none");
    let replaced = frame(&runtime, 50.0);
    let _lease = present(&mut runtime, replaced);
    assert_eq!(text(&runtime, "out"), "50 none");
    assert!(!node(&runtime, "plain").children[0].measure_content);

    // a key that leaves and returns is a new widget the handle never reads.
    click(&mut runtime, "drop");
    runtime.take_geometry_refresh();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "none none");
    click(&mut runtime, "restore");
    let restored = frame(&runtime, 60.0);
    let _lease = present(&mut runtime, restored);
    assert_eq!(text(&runtime, "out"), "none none");
}

#[test]
fn geometry_that_feeds_its_own_layout_stops_after_a_bounded_chain() {
    let (_directory, mut runtime) = load(
        r#"
        local nudge = ui.state(0)
        local geometry
        local box = ui.row { key = 'box', style = { width = function()
            nudge:get()
            local layout = geometry and geometry:layout()
            return if layout and layout.width == 50 then 80 else 50
        end } }
        geometry = ui.geometry(box)
        return ui.panel { children = {
            box,
            ui.button { key = 'nudge', on_click = function() nudge:set(nudge:get() + 1) end },
        } }
        "#,
    );
    let run = |runtime: &mut Runtime, frames: usize| {
        let mut changed = 0;
        let mut frozen = Vec::new();
        for _ in 0..frames {
            let width = f64::from(node(runtime, "box").style.width.unwrap());
            let frame = Frame::default()
                .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
                .with(placed(node(runtime, "box").id, 0.0, 0.0, width, 30.0));
            let (_lease, update) = present(runtime, frame);
            changed += usize::from(update.changed);
            frozen.extend(update.frozen);
        }
        (changed, frozen)
    };
    let (changed, frozen) = run(&mut runtime, 40);
    assert_eq!(changed, MAX_FEEDBACK_ROUNDS as usize);
    assert_eq!(frozen, vec![node(&runtime, "box").id]);
    assert!(!runtime.take_geometry_refresh());

    // an outside change starts one new bounded chain.
    click(&mut runtime, "nudge");
    let (changed, frozen) = run(&mut runtime, 40);
    assert_eq!(changed, MAX_FEEDBACK_ROUNDS as usize);
    assert_eq!(frozen.len(), 1);
}

#[test]
fn a_popup_sized_to_its_content_settles_in_one_extra_frame() {
    let (_directory, mut runtime) = load(
        r#"
        local label = ui.text { key = 'label', text = 'a longer menu entry' }
        local measured = ui.geometry(label)
        return ui.panel { children = { ui.button { key = 'open', children = { ui.popup {
            key = 'menu',
            style = { width = function()
                local content = measured:content()
                return if content then content.width + 24 else 120
            end },
            children = { label },
        } } } } }
        "#,
    );
    let menu = node(&runtime, "menu").id;
    assert_eq!(node(&runtime, "menu").style.width, Some(120.0));
    let mut changed = Vec::new();
    for _ in 0..4 {
        let width = f64::from(node(&runtime, "menu").style.width.unwrap());
        let frame = Frame {
            origin: Some((40.0, 30.0)),
            ..Frame::default()
        }
        .with(placed(menu, 0.0, 0.0, width, 40.0))
        .with(Placed {
            content: Some((150.0, 18.0)),
            ..placed(node(&runtime, "label").id, 12.0, 11.0, width - 24.0, 18.0)
        });
        let (_lease, update) = present_at(&mut runtime, menu, frame);
        assert!(update.frozen.is_empty());
        changed.push(update.changed);
    }
    assert_eq!(node(&runtime, "menu").style.width, Some(174.0));
    assert_eq!(changed, [true, true, false, false]);
}

#[test]
fn rejected_frames_and_refreshes_keep_the_accepted_geometry() {
    let (_directory, mut runtime) = load(
        r#"
        local box = ui.row { key = 'box' }
        local geometry = ui.geometry(box)
        return ui.panel { children = { box, ui.text { key = 'out', text = function()
            local layout = geometry:layout()
            if layout and layout.width > 100 then error('too wide') end
            return if layout then tostring(layout.width) else 'none'
        end } } }
        "#,
    );
    let frame = |runtime: &Runtime, width: f64| {
        Frame::default()
            .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
            .with(placed(node(runtime, "box").id, 0.0, 0.0, width, 30.0))
    };
    let first = frame(&runtime, 60.0);
    let _lease = present(&mut runtime, first);
    assert_eq!(text(&runtime, "out"), "60");

    // invalid measurements are rejected before any handle changes.
    let invalid = frame(&runtime, f64::NAN);
    assert!(runtime.publish_geometry(root(&runtime), &invalid).is_err());
    assert!(!runtime.take_geometry_refresh());

    // a dependent that rejects the new geometry leaves the accepted tree.
    let wide = Rc::new(frame(&runtime, 200.0));
    runtime
        .publish_geometry(root(&runtime), wide.as_ref())
        .unwrap();
    assert!(runtime.take_geometry_refresh());
    assert!(runtime.refresh().is_err());
    assert_eq!(text(&runtime, "out"), "60");
    let narrow = frame(&runtime, 90.0);
    let _lease = present(&mut runtime, narrow);
    assert_eq!(text(&runtime, "out"), "90");
}

#[test]
fn reloads_and_closed_surfaces_start_unavailable() {
    let (_directory, path) = fixture(LABEL);
    let mut runtime = Runtime::load(&path).unwrap();
    let frame = label_frame(&runtime, 40.0);
    let _lease = present(&mut runtime, frame);
    assert_eq!(text(&runtime, "out"), "10 40 80 40x20");

    runtime.forget_presented_geometry(root(&runtime)).unwrap();
    assert!(runtime.take_geometry_refresh());
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "none");

    let frame = label_frame(&runtime, 40.0);
    let _lease = present(&mut runtime, frame);
    runtime.reload(&path).unwrap();
    assert_eq!(text(&runtime, "out"), "none");
    assert!(!runtime.take_geometry_refresh());
}

#[test]
fn handles_are_created_only_outside_property_functions_and_are_bounded() {
    for source in [
        "local w = ui.row {} return ui.panel { children = { w, ui.text { text = function() ui.geometry(w) return '' end } } }",
        // asking again for a handle that exists is refused there too.
        "local w = ui.row {} local g = ui.geometry(w) return ui.panel { children = { w, ui.text { text = function() ui.geometry(w) return '' end } } }",
        // 65 handles that are all still referenced exceed the live limit.
        "local w, g = {}, {} for i = 1, 65 do w[i] = ui.row {} g[i] = ui.geometry(w[i]) end return ui.panel { children = w }",
        "local w, g = {}, {} for i = 1, 65 do w[i] = ui.row {} g[i] = ui.geometry(w[i]).layout end return ui.panel { children = w }",
        "ui.geometry({}) return ui.panel {}",
        "ui.geometry('label') return ui.panel {}",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}

const CONVERT: &str = r#"
    local result = ui.state('')
    local a = ui.row { key = 'a' }
    local b = ui.row { key = 'b' }
    local flat = ui.row { key = 'flat' }
    local inner = ui.row { key = 'inner' }
    local other = ui.row { key = 'other' }
    local cases = {
        ab = { a, 1, 2, b },
        inner_a = { inner, 0, 0, a },
        a_inner = { a, 45, 35, inner },
        other = { a, 0, 0, other },
        flat = { a, 0, 0, flat },
    }
    local buttons = {}
    for name, case in cases do
        table.insert(buttons, ui.button { key = 'do_' .. name, on_click = function()
            local ok, value = pcall(ui.convert, case[1], case[2], case[3], case[4])
            if not ok then
                result:set('error: ' .. tostring(value))
            elseif value == nil then
                result:set('nil')
            else
                result:set(string.format('%g %g', value.x, value.y))
            end
        end })
    end
    return {
        ui.panel { key = 'bar', children = {
            a, b, flat,
            ui.button { key = 'opener', children = { ui.popup { key = 'popup', children = { inner } } } },
            ui.column { children = buttons },
            ui.text { key = 'out', text = function() return result:get() end },
        } },
        ui.panel { key = 'dock', children = { other } },
    }
"#;

fn convert(runtime: &mut Runtime, case: &str) -> String {
    click(runtime, &format!("do_{case}"));
    text(runtime, "out")
}

#[test]
fn points_convert_between_widgets_on_related_surfaces_only() {
    let (_directory, mut runtime) = load(CONVERT);
    let bar = runtime.surfaces()[0].root.id;
    let popup = node(&runtime, "popup").id;
    assert_eq!(convert(&mut runtime, "ab"), "nil");
    let frame = Frame::default()
        .with(placed(bar, 0.0, 0.0, 800.0, 30.0))
        .with(placed(node(&runtime, "a").id, 10.0, 0.0, 40.0, 30.0))
        .with(Placed {
            scale: 2.0,
            ..placed(node(&runtime, "b").id, 100.0, 0.0, 40.0, 30.0)
        })
        .with(Placed {
            scale: 0.0,
            ..placed(node(&runtime, "flat").id, 200.0, 0.0, 40.0, 30.0)
        });
    let _bar = present_at(&mut runtime, bar, frame);
    assert_eq!(convert(&mut runtime, "ab"), "-44.5 1");
    // the popup has not presented, so its widgets have no position yet.
    assert_eq!(convert(&mut runtime, "inner_a"), "nil");
    let frame = Frame {
        origin: Some((50.0, 30.0)),
        ..Frame::default()
    }
    .with(placed(popup, 0.0, 0.0, 100.0, 100.0))
    .with(placed(node(&runtime, "inner").id, 5.0, 5.0, 20.0, 20.0));
    let popup_lease = present_at(&mut runtime, popup, frame);
    assert_eq!(convert(&mut runtime, "inner_a"), "45 35");
    assert_eq!(convert(&mut runtime, "a_inner"), "0 0");
    assert!(
        convert(&mut runtime, "other").contains("unrelated surfaces"),
        "{}",
        text(&runtime, "out")
    );
    let flat = convert(&mut runtime, "flat");
    assert!(flat.contains("not invertible"), "{flat}");
    drop(popup_lease);
    assert_eq!(convert(&mut runtime, "inner_a"), "nil");
}

#[test]
fn conversion_is_limited_to_event_handlers() {
    let (_directory, path) = fixture(
        "local w = ui.row {} return ui.panel { children = { w, ui.text { text = function() return tostring(ui.convert(w, 0, 0, w)) end } } }",
    );
    let error = Runtime::load(path).err().unwrap().to_string();
    assert!(error.contains("only inside event handlers"), "{error}");
}

#[test]
fn nested_popups_convert_through_every_origin_once_each_has_presented() {
    let (_directory, mut runtime) = load(
        r#"
        local result = ui.state('')
        local a = ui.row { key = 'a' }
        local inner = ui.row { key = 'inner' }
        local deep = ui.row { key = 'deep' }
        local cases = {
            deep_a = { deep, 0, 0, a },
            a_deep = { a, 62, 73, deep },
            deep_inner = { deep, 0, 0, inner },
        }
        local buttons = {}
        for name, case in cases do
            table.insert(buttons, ui.button { key = 'do_' .. name, on_click = function()
                local value = ui.convert(case[1], case[2], case[3], case[4])
                result:set(if value then string.format('%g %g', value.x, value.y) else 'nil')
            end })
        end
        return ui.panel { children = {
            a,
            ui.button { key = 'opener', children = { ui.popup { key = 'popup', children = {
                inner,
                ui.button { key = 'deeper', children = { ui.popup { key = 'nested', children = { deep } } } },
            } } } },
            ui.column { children = buttons },
            ui.text { key = 'out', text = function() return result:get() end },
        } }
        "#,
    );
    let bar = root(&runtime);
    let popup = node(&runtime, "popup").id;
    let nested = node(&runtime, "nested").id;
    let bar_frame = Frame::default()
        .with(placed(bar, 0.0, 0.0, 800.0, 30.0))
        .with(placed(node(&runtime, "a").id, 10.0, 0.0, 40.0, 30.0));
    let _bar = present_at(&mut runtime, bar, bar_frame);
    let popup_frame = Frame {
        origin: Some((50.0, 30.0)),
        ..Frame::default()
    }
    .with(placed(popup, 0.0, 0.0, 100.0, 100.0))
    .with(placed(node(&runtime, "inner").id, 5.0, 5.0, 20.0, 20.0));
    let _popup = present_at(&mut runtime, popup, popup_frame);
    // the nested popup's own frame is still missing.
    assert_eq!(convert(&mut runtime, "deep_a"), "nil");
    let nested_frame = Frame {
        origin: Some((20.0, 40.0)),
        ..Frame::default()
    }
    .with(placed(nested, 0.0, 0.0, 30.0, 30.0))
    .with(placed(node(&runtime, "deep").id, 2.0, 3.0, 10.0, 10.0));
    let nested_lease = present_at(&mut runtime, nested, nested_frame);
    // deep (2, 3) + nested (20, 40) + popup (50, 30) - a (10, 0).
    assert_eq!(convert(&mut runtime, "deep_a"), "62 73");
    assert_eq!(convert(&mut runtime, "a_deep"), "0 0");
    // a shared popup ancestor skips the panel: (22, 43) - inner (5, 5).
    assert_eq!(convert(&mut runtime, "deep_inner"), "17 38");
    drop(nested_lease);
    assert_eq!(convert(&mut runtime, "deep_inner"), "nil");
}

/// a box that flips between two widths whenever its own geometry changes,
/// beside a second observed widget the test can make invalid.
const TWO_HANDLES: &str = r#"
    local geometry
    local box = ui.row { key = 'box', style = { width = function()
        local layout = geometry and geometry:layout()
        return if layout and layout.width == 50 then 80 else 50
    end } }
    geometry = ui.geometry(box)
    local other = ui.row { key = 'other' }
    local second = ui.geometry(other)
    local evaluations = 0
    return ui.panel { children = {
        box,
        other,
        ui.text { key = 'out', text = function()
            evaluations += 1
            local layout = second:layout()
            return string.format('%d %s', evaluations, if layout then tostring(layout.width) else 'none')
        end },
    } }
"#;

fn two_handle_frame(runtime: &Runtime, other: f64, content: Option<(f64, f64)>) -> Frame {
    let width = f64::from(node(runtime, "box").style.width.unwrap());
    Frame::default()
        .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
        .with(placed(node(runtime, "box").id, 0.0, 0.0, width, 30.0))
        .with(Placed {
            content,
            ..placed(node(runtime, "other").id, 0.0, 0.0, other, 30.0)
        })
}

#[test]
fn a_frame_rejected_by_a_later_handle_changes_no_handle_chain_or_revision() {
    let (_directory, mut runtime) = load(TWO_HANDLES);
    // seven changing rounds, one short of the feedback limit.
    for round in 0..MAX_FEEDBACK_ROUNDS - 1 {
        let frame = two_handle_frame(&runtime, 10.0, None);
        let (_lease, update) = present(&mut runtime, frame);
        assert!(update.changed, "round {round}");
    }
    assert_eq!(text(&runtime, "out"), "2 10");
    let width = node(&runtime, "box").style.width;

    // the box would change validly, but the later handle's measurement is
    // invalid in each of these frames.
    for invalid in [
        two_handle_frame(&runtime, f64::NAN, None),
        two_handle_frame(&runtime, -1.0, None),
        two_handle_frame(&runtime, 10.0, Some((-1.0, 0.0))),
        two_handle_frame(&runtime, 10.0, Some((f64::INFINITY, 0.0))),
    ] {
        assert!(runtime.publish_geometry(root(&runtime), &invalid).is_err());
        assert!(!runtime.take_geometry_refresh());
        runtime.refresh().unwrap();
        assert_eq!(text(&runtime, "out"), "2 10");
        assert_eq!(node(&runtime, "box").style.width, width);
    }

    // the box's snapshot is unchanged, so the same valid frame still
    // changes it, as the eighth round; the chain then holds.
    let frame = two_handle_frame(&runtime, 10.0, None);
    let (_lease, update) = present(&mut runtime, frame);
    assert_eq!(
        update,
        GeometryUpdate {
            changed: true,
            frozen: Vec::new()
        }
    );
    let frame = two_handle_frame(&runtime, 10.0, None);
    let (_lease, update) = present(&mut runtime, frame);
    assert!(!update.changed);
    assert_eq!(update.frozen, [node(&runtime, "box").id]);
    assert_eq!(text(&runtime, "out"), "2 10");
}

#[test]
fn dropped_handles_are_reclaimed_so_a_dynamic_shell_never_runs_out() {
    let (_directory, mut runtime) = load(
        r#"
        local count = ui.state(0)
        local watched = ui.state(0)
        local latest, current
        return ui.panel { children = {
            ui.column { key = 'list', children = function()
                latest = ui.text { key = 'item' .. count:get(), text = 'x' }
                return { latest }
            end },
            ui.button { key = 'next', on_click = function() count:set(count:get() + 1) end },
            ui.button { key = 'watch', on_click = function()
                current = ui.geometry(latest)
                watched:set(watched:get() + 1)
            end },
            ui.button { key = 'burst', on_click = function()
                for _ = 1, 1000 do ui.geometry(ui.row {}) end
            end },
            ui.text { key = 'out', text = function()
                watched:get()
                return if current and current:available() then tostring(current:layout().width) else 'none'
            end },
        } }
        "#,
    );
    // far more widgets than the live limit, observed one after another.
    for index in 1..=200 {
        click(&mut runtime, "next");
        click(&mut runtime, "watch");
        let item = node(&runtime, &format!("item{index}"));
        assert!(item.measure_content);
        let frame = Frame::default()
            .with(placed(root(&runtime), 0.0, 0.0, 800.0, 30.0))
            .with(placed(item.id, 0.0, 0.0, f64::from(index), 10.0));
        let _lease = present(&mut runtime, frame);
        assert_eq!(text(&runtime, "out"), index.to_string());
    }
    // discarded handles return their revision signals. leaking one per
    // handle would exceed the configuration's 10,000 state values here.
    for _ in 0..11 {
        click(&mut runtime, "burst");
    }
    assert_eq!(text(&runtime, "out"), "200");
}

#[test]
fn a_kept_method_outlives_its_table_and_never_reads_a_recycled_widget() {
    let (_directory, mut runtime) = load(
        r#"
        local plain = ui.state('first')
        local watched = ui.state(0)
        local widgets = {}
        local kept, fresh
        local function describe(read)
            if not read then return '-' end
            local layout = read()
            return if layout then tostring(layout.width) else 'none'
        end
        return ui.panel { children = {
            ui.column { key = 'plain', children = function()
                widgets.plain = ui.text { text = plain:get() }
                return { widgets.plain }
            end },
            ui.button { key = 'watch', on_click = function()
                -- only the method survives; the handle table is dropped.
                kept = ui.geometry(widgets.plain).layout
                watched:set(watched:get() + 1)
            end },
            ui.button { key = 'replace', on_click = function() plain:set('second') end },
            ui.button { key = 'churn', on_click = function()
                for _ = 1, 200 do ui.geometry(ui.row {}) end
                fresh = ui.geometry(widgets.plain).layout
                watched:set(watched:get() + 1)
            end },
            ui.text { key = 'out', text = function()
                watched:get()
                return describe(kept) .. ' ' .. describe(fresh)
            end },
        } }
        "#,
    );
    let frame = |runtime: &Runtime, width: f64| {
        let plain = node(runtime, "plain").children[0].id;
        Frame::default()
            .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
            .with(placed(plain, 0.0, 0.0, width, 10.0))
    };
    click(&mut runtime, "watch");
    let first = frame(&runtime, 30.0);
    let _lease = present(&mut runtime, first);
    assert_eq!(text(&runtime, "out"), "30 -");

    // churning through many handles reclaims none that luau still holds.
    click(&mut runtime, "churn");
    let second = frame(&runtime, 31.0);
    let _lease = present(&mut runtime, second);
    assert_eq!(text(&runtime, "out"), "31 31");

    // the unkeyed replacement reuses the node id, and every slot and
    // revision freed above may be reused, yet the kept method stays retired.
    let id = node(&runtime, "plain").children[0].id;
    click(&mut runtime, "replace");
    assert_eq!(node(&runtime, "plain").children[0].id, id);
    assert!(runtime.take_geometry_refresh());
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime, "out"), "none none");
    click(&mut runtime, "churn");
    let replaced = frame(&runtime, 50.0);
    let _lease = present(&mut runtime, replaced);
    assert_eq!(text(&runtime, "out"), "none 50");
}

/// advances animations and presents one frame per step, as the host does,
/// and counts publications that changed or froze handles.
fn animate(
    runtime: &mut Runtime,
    frames: usize,
    frame: impl Fn(&Runtime) -> (NodeId, Frame),
) -> (usize, Vec<NodeId>) {
    let mut changed = 0;
    let mut frozen = Vec::new();
    for _ in 0..frames {
        runtime.advance(std::time::Instant::now()).unwrap();
        let (surface, frame) = frame(runtime);
        let (_lease, update) = present_at(runtime, surface, frame);
        changed += usize::from(update.changed);
        frozen.extend(update.frozen);
    }
    (changed, frozen)
}

#[test]
fn geometry_that_keeps_retargeting_its_own_transition_is_still_bounded() {
    let (_directory, mut runtime) = load(
        r#"
        local geometry
        local box = ui.row { key = 'box', style = { width = ui.transition(function()
            local layout = geometry and geometry:layout()
            return if layout then layout.width + 10 else 50
        end, { duration = 1000, easing = 'linear' }) } }
        geometry = ui.geometry(box)
        return ui.panel { children = { box } }
        "#,
    );
    // every animation frame moves the box, and every move retargets the
    // transition, so the animation would never end on its own.
    let (changed, frozen) = animate(&mut runtime, 120, |runtime| {
        let width = f64::from(node(runtime, "box").style.width.unwrap());
        let frame = Frame::default()
            .with(placed(root(runtime), 0.0, 0.0, 800.0, 30.0))
            .with(placed(node(runtime, "box").id, 0.0, 0.0, width, 30.0));
        (root(runtime), frame)
    });
    assert!(changed <= MAX_FEEDBACK_ROUNDS as usize + 1, "{changed}");
    assert_eq!(frozen, [node(&runtime, "box").id]);
    assert!(!runtime.take_geometry_refresh());
}

#[test]
fn an_animation_that_geometry_started_but_does_not_steer_is_not_frozen() {
    let (_directory, mut runtime) = load(
        r#"
        local label = ui.text { key = 'label', text = 'a longer menu entry' }
        local measured = ui.geometry(label)
        return ui.panel { children = { ui.button { key = 'open', children = { ui.popup {
            key = 'menu',
            style = { width = ui.transition(function()
                local content = measured:content()
                return if content then content.width + 24 else 120
            end, { duration = 1000, easing = 'linear' }) },
            children = { label },
        } } } } }
        "#,
    );
    // the label's allocated width follows the menu on every frame, while its
    // content size, and so the transition's target, stays the same.
    let frames = 40;
    let (changed, frozen) = animate(&mut runtime, frames, |runtime| {
        let menu = node(runtime, "menu").id;
        let width = f64::from(node(runtime, "menu").style.width.unwrap());
        let frame = Frame {
            origin: Some((40.0, 30.0)),
            ..Frame::default()
        }
        .with(placed(menu, 0.0, 0.0, width, 40.0))
        .with(Placed {
            content: Some((150.0, 18.0)),
            ..placed(node(runtime, "label").id, 12.0, 11.0, width - 24.0, 18.0)
        });
        (menu, frame)
    });
    assert!(frozen.is_empty(), "{frozen:?}");
    assert!(changed > MAX_FEEDBACK_ROUNDS as usize + 1, "{changed}");
    assert!(node(&runtime, "menu").style.width.unwrap() > 120.0);
}

#[test]
fn feedback_through_two_surfaces_stops_while_each_redraws_the_other() {
    let (_directory, mut runtime) = load(
        r#"
        local panel_geometry, popup_geometry
        local panel_box = ui.row { key = 'panel_box', style = { width = function()
            local layout = popup_geometry and popup_geometry:layout()
            return if layout then layout.width + 1 else 20
        end } }
        local popup_box = ui.row { key = 'popup_box', style = { width = function()
            local layout = panel_geometry and panel_geometry:layout()
            return if layout then layout.width + 1 else 20
        end } }
        panel_geometry = ui.geometry(panel_box)
        popup_geometry = ui.geometry(popup_box)
        return ui.panel { children = {
            panel_box,
            ui.button { key = 'open', children = { ui.popup { key = 'menu', children = { popup_box } } } },
        } }
        "#,
    );
    let bar = root(&runtime);
    let menu = node(&runtime, "menu").id;
    let panel_frame = |runtime: &Runtime| {
        let width = f64::from(node(runtime, "panel_box").style.width.unwrap());
        Frame::default()
            .with(placed(bar, 0.0, 0.0, 800.0, 30.0))
            .with(placed(node(runtime, "panel_box").id, 0.0, 0.0, width, 30.0))
    };
    let popup_frame = |runtime: &Runtime| {
        let width = f64::from(node(runtime, "popup_box").style.width.unwrap());
        Frame::default()
            .with(placed(menu, 0.0, 0.0, 100.0, 60.0))
            .with(placed(node(runtime, "popup_box").id, 0.0, 0.0, width, 30.0))
    };
    // a refresh redraws every surface of the output, so each surface also
    // presents an unchanged frame between the ones that change it.
    let run = |runtime: &mut Runtime, rounds: usize| {
        let mut changed = 0;
        for _ in 0..rounds {
            for (surface, frame) in [
                (bar, panel_frame(runtime)),
                (bar, panel_frame(runtime)),
                (menu, popup_frame(runtime)),
                (menu, popup_frame(runtime)),
            ] {
                let (_lease, update) = present_at(runtime, surface, frame);
                changed += usize::from(update.changed);
            }
        }
        changed
    };
    let changed = run(&mut runtime, 40);
    assert!(changed <= 2 * MAX_FEEDBACK_ROUNDS as usize + 2, "{changed}");
    assert_eq!(run(&mut runtime, 10), 0);
}

#[test]
fn geometry_that_follows_a_scroll_is_not_mistaken_for_feedback() {
    let (_directory, mut runtime) = load(
        r#"
        local box = ui.row { key = 'box' }
        local measured = ui.geometry(box)
        return ui.panel { children = {
            ui.scroll { children = { box } },
            ui.text { key = 'badge', text = function()
                local layout = measured:layout()
                return if layout then tostring(layout.y) else 'none'
            end },
        } }
        "#,
    );
    // every step moves the box and, through the badge, the tree, as an
    // overlay that follows a scrolled widget does.
    for step in 0..40 {
        let y = 100.0 - f64::from(step);
        let frame = Frame {
            moved: step > 0,
            ..Frame::default()
        }
        .with(placed(root(&runtime), 0.0, 0.0, 800.0, 30.0))
        .with(placed(node(&runtime, "box").id, 0.0, y, 40.0, 10.0));
        let (_lease, update) = present(&mut runtime, frame);
        assert_eq!(
            update,
            GeometryUpdate {
                changed: true,
                frozen: Vec::new()
            },
            "step {step}"
        );
        assert_eq!(text(&runtime, "badge"), y.to_string());
    }
}
