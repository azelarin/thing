use std::{fs, rc::Rc};

use tempfile::TempDir;
use thing_core::{Node, NodeId};
use thing_runtime::{BoundsProvider, BoundsRect, Event, Runtime, WidgetBounds};

const SOURCE: &str = r#"
    local result = ui.state('')
    local width = ui.state(60)
    local target = ui.row { key = 'target', style = { width = function() return width:get() end } }
    return ui.panel { children = {
        target,
        ui.button { key = 'read', on_click = function()
            width:set(100)
            local bounds = ui.bounds(target)
            if not bounds then result:set('none'); return end
            local valid = pcall(function() bounds.layout.width = 0 end)
            assert(not valid)
            valid = pcall(function() bounds.surface = 'changed' end)
            assert(not valid)
            result:set(string.format('%g %g %g %g', bounds.layout.x,
                bounds.layout.width, bounds.visual.x, bounds.visual.width))
        end },
        ui.text { key = 'result', text = function() return result:get() end },
    } }
"#;

fn load(source: &str) -> (TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
        if node.key.as_deref() == Some(key) {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, key))
    }
    runtime
        .surfaces()
        .iter()
        .find_map(|surface| find(&surface.root, key))
        .unwrap()
}

fn rect(x: f64, width: f64) -> BoundsRect {
    BoundsRect {
        x,
        y: 0.0,
        width,
        height: 20.0,
    }
}

fn boxes(node: NodeId, x: f64, width: f64) -> WidgetBounds {
    WidgetBounds {
        node,
        layout: rect(x, width),
        visual: rect(x * 2.0, width * 2.0),
    }
}

struct Boxes(Vec<WidgetBounds>);

impl BoundsProvider for Boxes {
    fn get(&self, node: NodeId) -> Option<WidgetBounds> {
        self.0.iter().find(|bounds| bounds.node == node).copied()
    }
}

fn provider(boxes: Vec<WidgetBounds>) -> Rc<dyn BoundsProvider> {
    Rc::new(Boxes(boxes))
}

fn frame(runtime: &Runtime, width: f64) -> Rc<dyn BoundsProvider> {
    provider(vec![
        boxes(runtime.root().unwrap().id, 0.0, 800.0),
        boxes(node(runtime, "target").id, 20.0, width),
    ])
}

fn read(runtime: &mut Runtime) -> String {
    runtime
        .dispatch(node(runtime, "read").id, Event::Click)
        .unwrap();
    node(runtime, "result").text.clone()
}

#[test]
fn only_presented_boxes_are_read_and_retired_frames_become_unavailable() {
    let (_directory, mut runtime) = load(SOURCE);
    assert_eq!(read(&mut runtime), "none");
    let owner = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 60.0))
        .unwrap();
    assert_eq!(read(&mut runtime), "20 60 40 120");
    assert_eq!(node(&runtime, "target").style.width, Some(100.0));
    assert_eq!(read(&mut runtime), "20 60 40 120");
    drop(owner);
    assert_eq!(read(&mut runtime), "none");
}

#[test]
fn replacing_a_snapshot_retires_the_previous_boxes() {
    let (_directory, mut runtime) = load(SOURCE);
    let old = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 60.0))
        .unwrap();
    assert_eq!(read(&mut runtime), "20 60 40 120");
    let new = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 100.0))
        .unwrap();
    assert_eq!(read(&mut runtime), "20 100 40 200");
    drop(new);
    assert_eq!(read(&mut runtime), "none");
    drop(old);
}

#[test]
fn invalid_root_geometry_does_not_replace_the_accepted_snapshot() {
    let (_directory, mut runtime) = load(SOURCE);
    let _owner = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 60.0))
        .unwrap();
    let mut invalid = vec![boxes(runtime.root().unwrap().id, 0.0, 800.0)];
    invalid[0].visual.width = f64::NAN;
    assert!(
        runtime
            .record_presented_bounds(runtime.root().unwrap().id, provider(invalid))
            .is_err()
    );
    assert_eq!(read(&mut runtime), "20 60 40 120");
}

#[test]
fn reloads_do_not_reuse_an_old_runtime_measurement() {
    let (directory, mut runtime) = load(SOURCE);
    let path = directory.path().join("panel.luau");
    let _old = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 60.0))
        .unwrap();
    fs::write(&path, "error('bad edit')").unwrap();
    assert!(runtime.reload(&path).is_err());
    assert_eq!(read(&mut runtime), "20 60 40 120");
    fs::write(&path, SOURCE).unwrap();
    runtime.reload(&path).unwrap();
    assert_eq!(read(&mut runtime), "none");
}

#[test]
fn bounds_are_not_a_layout_property_or_load_time_query() {
    for source in [
        "local widget=ui.row {}; ui.bounds(widget); return ui.panel { children={widget} }",
        "local widget=ui.row {}; return ui.panel { children={widget}, style={width=function() ui.bounds(widget); return 1 end} }",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        let error = Runtime::load(path).err().expect("bounds query must fail");
        assert!(error.to_string().contains("only inside event handlers"));
    }
}

#[test]
fn keyed_list_rebuilds_keep_measurements_for_retained_widgets() {
    let (_directory, mut runtime) = load(
        r#"
        local revision = ui.state(0)
        local result = ui.state('')
        local target = ui.row { key = 'target' }
        local read = ui.button { key = 'read', on_click = function()
            local bounds = ui.bounds(target)
            result:set(if bounds then tostring(bounds.visual.width) else 'none')
        end }
        local rebuild = ui.button { key = 'rebuild',
            on_click = function() revision:set(revision:get() + 1) end }
        return ui.panel { children = function()
            local _ = revision:get()
            return { target, read, rebuild,
                ui.text { key = 'result', text = function() return result:get() end } }
        end }
    "#,
    );
    let _old = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 60.0))
        .unwrap();
    assert_eq!(read(&mut runtime), "120");
    runtime
        .dispatch(node(&runtime, "rebuild").id, Event::Click)
        .unwrap();
    assert_eq!(read(&mut runtime), "120");
    let _new = runtime
        .record_presented_bounds(runtime.root().unwrap().id, frame(&runtime, 80.0))
        .unwrap();
    assert_eq!(read(&mut runtime), "160");
}

#[test]
fn popup_coordinates_cannot_be_read_as_parent_coordinates() {
    let (_directory, mut runtime) = load(
        r#"
        local result = ui.state('')
        local inside = ui.text { key = 'inside', text = 'popup' }
        return ui.panel { children = {
            ui.button { key = 'read', on_click = function()
                local bounds = ui.bounds(inside)
                result:set(if bounds then tostring(bounds.visual.width) else 'none')
            end, children = { ui.popup { key = 'popup', children = { inside } } } },
            ui.text { key = 'result', text = function() return result:get() end },
        } }
    "#,
    );
    let root = runtime.root().unwrap().id;
    let popup = node(&runtime, "popup").id;
    let inside = node(&runtime, "inside").id;
    let _parent = runtime
        .record_presented_bounds(
            root,
            provider(vec![boxes(root, 0.0, 800.0), boxes(inside, 0.0, 40.0)]),
        )
        .unwrap();
    assert_eq!(read(&mut runtime), "none");
    let _popup = runtime
        .record_presented_bounds(
            popup,
            provider(vec![boxes(popup, 0.0, 80.0), boxes(inside, 0.0, 40.0)]),
        )
        .unwrap();
    assert_eq!(read(&mut runtime), "80");
}

#[test]
fn an_unkeyed_replacement_cannot_read_the_previous_widgets_box() {
    let (_directory, mut runtime) = load(
        r#"
        local revision = ui.state(0)
        local result = ui.state('')
        local target = nil
        local read = ui.button { key = 'read', on_click = function()
            local bounds = ui.bounds(target)
            result:set(if bounds then tostring(bounds.visual.width) else 'none')
        end }
        local rebuild = ui.button { key = 'rebuild',
            on_click = function() revision:set(revision:get() + 1) end }
        local text = ui.text { key = 'result', text = function() return result:get() end }
        return ui.panel { children = function()
            local _ = revision:get()
            target = ui.row {}
            return { target, read, rebuild, text }
        end }
    "#,
    );
    let root = runtime.root().unwrap().id;
    let target = runtime.root().unwrap().children[0].id;
    let _old = runtime
        .record_presented_bounds(
            root,
            provider(vec![boxes(root, 0.0, 800.0), boxes(target, 20.0, 60.0)]),
        )
        .unwrap();
    assert_eq!(read(&mut runtime), "120");
    runtime
        .dispatch(node(&runtime, "rebuild").id, Event::Click)
        .unwrap();
    assert_eq!(runtime.root().unwrap().children[0].id, target);
    assert_eq!(read(&mut runtime), "none");
    let _new = runtime
        .record_presented_bounds(
            root,
            provider(vec![boxes(root, 0.0, 800.0), boxes(target, 20.0, 80.0)]),
        )
        .unwrap();
    assert_eq!(read(&mut runtime), "160");
}
