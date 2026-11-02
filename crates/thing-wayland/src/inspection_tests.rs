use std::{rc::Rc, time::Duration};

use thing_core::{Kind, Node, NodeId};
use thing_ui::{Layout, Measurer, Size};

use super::*;
use crate::command::{HostRequest, parse_host_request, parse_request};

fn parse(line: &str) -> Result<HostRequest, String> {
    parse_host_request(line.as_bytes())
}

#[test]
fn inspection_is_a_separate_request_kind_beside_commands() {
    let request =
        parse(r#"{"version":1,"inspect":{"keys":true,"node":7,"highlight":2.5},"output":"DP-1"}"#)
            .unwrap();
    assert_eq!(
        request,
        HostRequest::Inspect(InspectRequest {
            selection: OutputSelection::Named("DP-1".into()),
            query: InspectQuery {
                keys: true,
                node: Some(7),
                highlight: Some(Duration::from_secs_f64(2.5)),
                ..InspectQuery::default()
            },
        })
    );
    // declared commands parse exactly as before, including one named inspect.
    let command = parse_request(br#"{"version":1,"command":"inspect","arguments":[1]}"#).unwrap();
    assert_eq!(command.command, "inspect");
    assert_eq!(command.selection, OutputSelection::Implicit);
}

#[test]
fn malformed_and_mixed_requests_are_rejected() {
    for (line, expected) in [
        (
            r#"{"version":1,"inspect":{},"command":"x"}"#,
            "either a command or an inspection",
        ),
        (
            r#"{"version":1,"inspect":{},"arguments":1}"#,
            "either a command or an inspection",
        ),
        (
            r#"{"version":1,"inspect":{},"extra":1}"#,
            "unknown request field",
        ),
        (
            r#"{"version":2,"inspect":{}}"#,
            "unsupported request version",
        ),
        (r#"{"version":1,"inspect":[]}"#, "must be a json object"),
        (
            r#"{"version":1,"inspect":{"text":true}}"#,
            "unknown inspect field",
        ),
        (
            r#"{"version":1,"inspect":{"keys":1}}"#,
            "keys must be a boolean",
        ),
        (
            r#"{"version":1,"inspect":{"node":0}}"#,
            "positive whole numbers",
        ),
        (
            r#"{"version":1,"inspect":{"node":-3}}"#,
            "positive whole numbers",
        ),
        (
            r#"{"version":1,"inspect":{"node":1.5}}"#,
            "positive whole numbers",
        ),
        (
            r#"{"version":1,"inspect":{"node":1,"at_pointer":true}}"#,
            "either a node or the pointer",
        ),
        (r#"{"version":1,"inspect":{"highlight":1}}"#, "needs a node"),
        (
            r#"{"version":1,"inspect":{"node":1,"highlight":0.05}}"#,
            "between 0.1 and 60",
        ),
        (
            r#"{"version":1,"inspect":{"node":1,"highlight":61}}"#,
            "between 0.1 and 60",
        ),
        (
            r#"{"version":1,"inspect":{"node":1,"highlight":1},"all_outputs":true}"#,
            "one output",
        ),
        (r#"{"version":1,"inspect":{"surface":""}}"#, "surface names"),
        (
            r#"{"version":1,"inspect":{},"output":"a","all_outputs":true}"#,
            "either one output or all outputs",
        ),
        (r#"{"version":1}"#, "needs a command name"),
    ] {
        let error = parse(line).unwrap_err();
        assert!(error.contains(expected), "{line}: {error}");
    }
}

#[test]
fn queries_round_trip_through_the_client_encoding() {
    let query = InspectQuery {
        at_pointer: true,
        surface: Some("bar".into()),
        highlight: Some(Duration::from_millis(100)),
        ..InspectQuery::default()
    };
    let line = serde_json::json!({ "version": 1, "inspect": encode_query(&query) }).to_string();
    assert_eq!(
        parse(&line).unwrap(),
        HostRequest::Inspect(InspectRequest {
            selection: OutputSelection::Implicit,
            query,
        })
    );
}

struct Fixed;

impl Measurer for Fixed {
    fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
        Size {
            width: 10.0,
            height: 10.0,
        }
    }
}

fn snapshot_of(root: &Node) -> FrameSnapshot {
    let layout = Layout::compute(
        root,
        Size {
            width: 400.0,
            height: 100.0,
        },
        &mut Fixed,
    )
    .unwrap();
    let (records, uncaptured) = capture(root);
    FrameSnapshot {
        layout: Rc::new(layout),
        consumer: String::new(),
        targets: Default::default(),
        records,
        uncaptured,
        size: (400, 100),
        scale: 1.0,
        origin: None,
    }
}

fn view(snapshot: &FrameSnapshot) -> SurfaceView<'_> {
    SurfaceView {
        name: "default",
        role: "panel",
        root: NodeId(1),
        snapshot,
        pointer: None,
    }
}

#[test]
fn capture_skips_popups_and_inherits_hidden_ancestors() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut hidden = Node::new(NodeId(2), Kind::Row);
    hidden.visible = false;
    hidden.children.push(Node::new(NodeId(3), Kind::Text));
    root.children.push(hidden);
    root.children.push(Node::new(NodeId(4), Kind::Popup));
    let (records, uncaptured) = capture(&root);
    let summary: Vec<_> = records
        .iter()
        .map(|record| (record.id.0, record.parent.map(|id| id.0), record.visible))
        .collect();
    assert_eq!(
        summary,
        [(1, None, true), (2, Some(1), false), (3, Some(2), false)]
    );
    assert_eq!(uncaptured, 0);
}

#[test]
fn reports_omit_text_and_keys_unless_keys_are_requested() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut label = Node::new(NodeId(2), Kind::Text);
    label.text = "hunter2".into();
    label.key = Some("account-\u{e9}".into());
    let mut field = Node::new(NodeId(3), Kind::TextInput);
    field.text = "typed secret".into();
    field.placeholder = "placeholder secret".into();
    root.children.push(label);
    root.children.push(field);
    let snapshot = snapshot_of(&root);
    let plain = describe_surface(&view(&snapshot), &Selection::All, false, &mut Budget::new())
        .unwrap()
        .to_string();
    for private in ["hunter2", "typed secret", "placeholder", "account"] {
        assert!(!plain.contains(private), "{plain}");
    }
    assert!(plain.contains(r#""kind":"text_input""#), "{plain}");
    let keyed = describe_surface(&view(&snapshot), &Selection::All, true, &mut Budget::new())
        .unwrap()
        .to_string();
    assert!(keyed.contains("account-\u{e9}"), "{keyed}");
    assert!(!keyed.contains("hunter2"), "{keyed}");
}

#[test]
fn long_keys_are_cut_at_character_boundaries() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    // 127 ascii bytes and a two byte character that crosses the limit.
    root.key = Some(format!("{}\u{e9}tail", "k".repeat(MAX_KEY_BYTES - 1)));
    let (records, _) = capture(&root);
    let (key, cut) = records[0].key.as_ref().unwrap();
    assert!(*cut);
    assert_eq!(key.len(), MAX_KEY_BYTES - 1);
}

#[test]
fn reports_stay_within_the_reply_limit_and_count_omissions() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut id = 2;
    for _ in 0..40 {
        let mut row = Node::new(NodeId(id), Kind::Row);
        id += 1;
        for _ in 0..40 {
            let mut child = Node::new(NodeId(id), Kind::Row);
            // control characters escape to six bytes each in json.
            child.key = Some(format!("{id:>8}{}", "\u{1}".repeat(MAX_KEY_BYTES - 8)));
            id += 1;
            row.children.push(child);
        }
        root.children.push(row);
    }
    let snapshot = snapshot_of(&root);
    assert_eq!(snapshot.records.len(), MAX_CAPTURED_NODES.min(1641));
    let mut budget = Budget::new();
    let mut sections = Vec::new();
    // several surfaces share one budget, as several outputs do.
    for _ in 0..4 {
        if let Some(section) =
            describe_surface(&view(&snapshot), &Selection::All, true, &mut budget)
        {
            sections.push(section);
        }
    }
    let listed: usize = sections
        .iter()
        .map(|section| section["nodes"].as_array().unwrap().len())
        .sum();
    let omitted: u64 = sections
        .iter()
        .filter_map(|section| section["omitted"].as_u64())
        .sum();
    assert!(listed > 0 && listed <= MAX_REPORT_NODES);
    // every widget of a listed surface is either reported or counted, and a
    // surface whose header no longer fits is left out whole for the host to
    // count.
    assert!(!sections.is_empty() && sections.len() < 4);
    assert_eq!(listed as u64 + omitted, sections.len() as u64 * 1641);
    let reply = encode_reply(&InspectReply::Reported(vec![OutputReport {
        output: Some("DP-1".into()),
        result: Ok(serde_json::json!({ "surfaces": sections })),
    }]));
    assert!(reply.len() < MAX_REPLY_BYTES, "{}", reply.len());
    assert!(!String::from_utf8(reply).unwrap().contains("exceeds"));
}

#[test]
fn filters_narrow_reports_to_a_subtree_or_a_picked_path() {
    let mut root = Node::new(NodeId(1), Kind::Panel);
    let mut row = Node::new(NodeId(2), Kind::Row);
    row.children.push(Node::new(NodeId(3), Kind::Text));
    root.children.push(row);
    root.children.push(Node::new(NodeId(4), Kind::Row));
    let snapshot = snapshot_of(&root);
    let ids = |selection| {
        let section =
            describe_surface(&view(&snapshot), &selection, false, &mut Budget::new()).unwrap();
        section["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["id"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(Selection::Subtree(NodeId(2))), [2, 3]);
    assert_eq!(ids(Selection::Path(NodeId(3))), [1, 2, 3]);
    assert_eq!(ids(Selection::All), [1, 2, 3, 4]);
}

#[test]
fn named_surface_keys_require_explicit_disclosure() {
    let root = Node::new(NodeId(1), Kind::Panel);
    let snapshot = snapshot_of(&root);
    let view = SurfaceView {
        name: "private-surface-sentinel",
        ..view(&snapshot)
    };
    let plain = describe_surface(&view, &Selection::All, false, &mut Budget::new()).unwrap();
    assert!(!plain.to_string().contains("private-surface-sentinel"));
    let keyed = describe_surface(&view, &Selection::All, true, &mut Budget::new()).unwrap();
    assert!(keyed.to_string().contains("private-surface-sentinel"));
    let default = SurfaceView {
        name: "default",
        ..view
    };
    let plain = describe_surface(&default, &Selection::All, false, &mut Budget::new()).unwrap();
    assert!(plain.get("surface").is_none());
}

#[test]
fn empty_outputs_cannot_exhaust_the_reply_envelope() {
    let mut budget = Budget::new();
    let mut reports = Vec::new();
    for _ in 0..10_000 {
        if !budget.charge_output(None) {
            break;
        }
        reports.push(OutputReport {
            output: None,
            result: Ok(serde_json::json!({ "surfaces": [] })),
        });
    }
    let expected = reports.len();
    let bytes = encode_reply(&InspectReply::Reported(reports));
    let reply: Json = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        reply.get("results").and_then(Json::as_array).map(Vec::len),
        Some(expected),
        "the budget must prevent the oversized-reply fallback: {reply}"
    );
    assert!(bytes.len() <= MAX_REPLY_BYTES);
}
