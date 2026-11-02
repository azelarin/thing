//! live inspection on a real host and private compositor: reports read only
//! presented frames, and highlights follow scoped widget identities.

use std::{
    fs,
    time::{Duration, Instant},
};

use serde_json::Value as Json;
use thing_runtime::CallValue;

use super::{super::wheel_tests::Host, Inspector};
use crate::{
    command::{CommandHost, CommandRequest, OutputSelection},
    inspection::{InspectQuery, InspectReply, InspectRequest},
};

const PANEL: &str = r#"
local width = ui.state(320)
local shown = ui.state(true)
ui.command('resize', function(arguments) width:set(arguments.width) end)
ui.command('hide', function() shown:set(false) end)
return ui.panel {
    style = { width = 640, height = 80 },
    children = { ui.row {
        key = 'bar',
        visible = function() return shown:get() end,
        style = { width = function() return width:get() end, height = 20 },
        children = { ui.text {
            key = 'label',
            text = 'hunter2',
            style = { width = 40, height = 20 },
        } },
    } },
}
"#;

fn start() -> Host {
    let mut host = Host::start_with(|_| PANEL.to_owned());
    host.app.inspector = Some(Inspector::default());
    // the first frame presented before inspection began; capture the next.
    for surface in &mut host.app.surfaces {
        surface.dirty = true;
    }
    settle(&mut host, |host| host.app.surfaces[0].inspection.is_some());
    host
}

fn settle(host: &mut Host, mut done: impl FnMut(&Host) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(host) {
        assert!(Instant::now() < deadline, "the host never settled");
        host.turn();
    }
}

fn inspect(host: &mut Host, query: InspectQuery) -> Result<Json, String> {
    let request = InspectRequest {
        selection: OutputSelection::Implicit,
        query,
    };
    match host.app.inspect(request) {
        InspectReply::Failed(error) => Err(error),
        InspectReply::Reported(mut reports) => {
            assert_eq!(reports.len(), 1);
            reports.remove(0).result
        }
    }
}

fn command(host: &mut Host, name: &str, arguments: CallValue) {
    let reply = host.app.run_command(CommandRequest {
        command: name.into(),
        arguments,
        selection: OutputSelection::Implicit,
    });
    assert!(
        matches!(&reply, crate::command::CommandReply::Ran(results) if results.iter().all(|result| result.result.is_ok())),
        "{reply:?}"
    );
}

fn bar(report: &Json) -> &Json {
    report["surfaces"][0]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "row")
        .expect("the bar is reported")
}

fn bar_id(host: &Host) -> u64 {
    let output = host.app.surfaces[0].output.clone();
    host.app.configurations.root(&output).unwrap().children[0]
        .id
        .0
}

#[test]
fn inspection_is_refused_unless_enabled() {
    let mut host = Host::start_with(|_| PANEL.to_owned());
    let error = inspect(&mut host, InspectQuery::default()).unwrap_err();
    assert!(error.contains("--inspector"), "{error}");
    assert!(host.app.surfaces[0].inspection.is_none());
}

#[test]
fn reports_show_the_presented_frame_until_an_accepted_update_presents() {
    let mut host = start();
    let report = inspect(&mut host, InspectQuery::default()).unwrap();
    assert_eq!(bar(&report)["bounds"][2], 320.0);

    // the update is accepted but not yet drawn: the report still pairs the
    // old tree's metadata with the old frame.
    let arguments = crate::command::parse_arguments(r#"{"width": 480}"#).unwrap();
    command(&mut host, "resize", arguments);
    let report = inspect(&mut host, InspectQuery::default()).unwrap();
    assert_eq!(bar(&report)["bounds"][2], 320.0);

    settle(&mut host, |host| {
        let surface = &host.app.surfaces[0];
        surface.inspection.as_ref().is_some_and(|snapshot| {
            snapshot
                .layout
                .semantic_placement(thing_core::NodeId(bar_id(host)))
                .is_some_and(|placement| placement.bounds.width == 480.0)
        })
    });
    let report = inspect(&mut host, InspectQuery::default()).unwrap();
    assert_eq!(bar(&report)["bounds"][2], 480.0);
}

#[test]
fn reports_never_carry_text_and_show_keys_only_on_request() {
    let mut host = start();
    let plain = inspect(&mut host, InspectQuery::default())
        .unwrap()
        .to_string();
    assert!(!plain.contains("hunter2"), "{plain}");
    assert!(!plain.contains("\"bar\""), "{plain}");
    assert!(!plain.contains("label"), "{plain}");
    let keyed = inspect(
        &mut host,
        InspectQuery {
            keys: true,
            ..InspectQuery::default()
        },
    )
    .unwrap()
    .to_string();
    assert!(keyed.contains("\"key\":\"bar\""), "{keyed}");
    assert!(!keyed.contains("hunter2"), "{keyed}");
}

fn highlight(host: &mut Host, node: u64, seconds: f64) {
    inspect(
        host,
        InspectQuery {
            node: Some(node),
            highlight: Some(Duration::from_secs_f64(seconds)),
            ..InspectQuery::default()
        },
    )
    .unwrap();
}

fn highlighted(host: &Host) -> bool {
    host.app
        .inspector
        .as_ref()
        .is_some_and(|inspector| inspector.deadline().is_some())
}

#[test]
fn highlights_expire_with_one_redraw() {
    let mut host = start();
    let id = bar_id(&host);
    highlight(&mut host, id, 0.1);
    assert!(highlighted(&host));
    // starting a highlight asks for a frame that shows it.
    assert!(host.app.surfaces[0].dirty);
    assert!(host.app.next_deadline().is_some());
    settle(&mut host, |host| !highlighted(host));
    // expiry asked for one more frame, which removes the overlay, and then
    // nothing keeps drawing.
    settle(&mut host, |host| {
        let surface = &host.app.surfaces[0];
        !surface.dirty && !surface.waiting
    });
    let quiet = Instant::now() + Duration::from_millis(200);
    while Instant::now() < quiet {
        host.turn();
        assert!(!host.app.surfaces[0].dirty);
    }
}

#[test]
fn hiding_the_widget_ends_its_highlight() {
    let mut host = start();
    let id = bar_id(&host);
    highlight(&mut host, id, 30.0);
    command(&mut host, "hide", CallValue::Nil);
    settle(&mut host, |host| !highlighted(host));
}

#[test]
fn rejected_reloads_keep_the_highlight_and_accepted_reloads_end_it() {
    let mut host = start();
    let id = bar_id(&host);
    highlight(&mut host, id, 30.0);
    let entry = host.app.configurations.entry().to_owned();
    let qh = host.app.queue.clone();

    fs::write(&entry, "return ui.panel { style = { width = 'wide' } }").unwrap();
    host.app.reload(&qh).unwrap();
    assert!(highlighted(&host));

    fs::write(&entry, PANEL).unwrap();
    host.app.reload(&qh).unwrap();
    assert!(!highlighted(&host));
}

#[test]
fn unknown_nodes_report_an_error_without_highlighting() {
    let mut host = start();
    let error = inspect(
        &mut host,
        InspectQuery {
            node: Some(999_999),
            highlight: Some(Duration::from_secs(1)),
            ..InspectQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("999999"), "{error}");
    assert!(!highlighted(&host));
}

#[test]
fn pointer_picking_is_passive() {
    let mut host = start();
    let error = inspect(
        &mut host,
        InspectQuery {
            at_pointer: true,
            ..InspectQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("pointer"), "{error}");

    // a wheel event moves the pointer over the bar's label.
    let output = host.app.surfaces[0].output.clone();
    let label = host.app.configurations.root(&output).unwrap().children[0].children[0].id;
    let bounds = host.app.surfaces[0]
        .layouts
        .presented()
        .and_then(|layout| layout.get(label))
        .unwrap()
        .border_aabb;
    let (x, y) = (
        f64::from((bounds.x + bounds.width / 2.0).round()),
        f64::from((bounds.y + bounds.height / 2.0).round()),
    );
    host.wheel(x, y, 0, 0);
    let before = (
        host.app.surfaces[0].input,
        host.app.surfaces[0].hover_path.clone(),
        host.app.pointer_capture.is_some(),
    );
    let report = inspect(
        &mut host,
        InspectQuery {
            at_pointer: true,
            ..InspectQuery::default()
        },
    )
    .unwrap();
    let section = &report["surfaces"][0];
    assert_eq!(section["pointer"], serde_json::json!([x, y]));
    let kinds: Vec<_> = section["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["kind"].as_str().unwrap())
        .collect();
    // the noninteractive label is found, with its ancestors.
    assert_eq!(kinds, ["panel", "row", "text"]);
    assert_eq!(section["picked"], section["nodes"][2]["id"],);
    let after = (
        host.app.surfaces[0].input,
        host.app.surfaces[0].hover_path.clone(),
        host.app.pointer_capture.is_some(),
    );
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
}

#[test]
fn an_unknown_surface_filter_reports_an_error() {
    let mut host = start();
    let error = inspect(
        &mut host,
        InspectQuery {
            surface: Some("missing".into()),
            ..InspectQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("--surface"), "{error}");
}
