//! live layout inspection: the wire request, frame snapshots, and bounded
//! reports.
//!
//! an inspection request shares the command socket and its limits but never
//! runs configuration code. reports read only frames the compositor was given,
//! together with the widget kinds and keys captured from the same tree, so a
//! newer accepted update never pairs with an older presented layout.

use std::{collections::BTreeMap, rc::Rc, time::Duration};

use serde_json::{Map, Value as Json};
use thing_core::{Kind, Node, NodeId, Rect};
use thing_runtime::UiTarget;
use thing_ui::Layout;

use crate::command::{MAX_REPLY_BYTES, OutputSelection};

/// widgets captured from one presented frame.
pub(crate) const MAX_CAPTURED_NODES: usize = 2048;
/// widget records in one reply, across every output and surface.
pub(crate) const MAX_REPORT_NODES: usize = 512;
/// bytes kept from a widget key; longer keys are cut at a character boundary.
pub(crate) const MAX_KEY_BYTES: usize = 128;
/// the longest surface name a request may filter by.
pub(crate) const MAX_SURFACE_NAME_BYTES: usize = 256;
/// the shortest and longest highlight.
pub(crate) const MIN_HIGHLIGHT: Duration = Duration::from_millis(100);
pub(crate) const MAX_HIGHLIGHT: Duration = Duration::from_secs(60);
// room for the reply's fixed envelope after every charged part.
const REPLY_RESERVE: usize = 4096;
// the largest exact integer json numbers carry.
const MAX_NODE_ID: u64 = (1 << 53) - 1;

/// what one inspection asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InspectQuery {
    /// include widget keys, which may carry user data.
    pub keys: bool,
    /// report the widget under the pointer instead of whole surfaces.
    pub at_pointer: bool,
    /// only the surfaces of this root, `default` for the unnamed one.
    pub surface: Option<String>,
    /// only this widget and its descendants.
    pub node: Option<u64>,
    /// highlight the selected widget for this long.
    pub highlight: Option<Duration>,
}

impl InspectQuery {
    pub fn validate(&self, selection: &OutputSelection) -> Result<(), String> {
        if self
            .surface
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.len() > MAX_SURFACE_NAME_BYTES)
        {
            return Err(format!(
                "surface names are 1 to {MAX_SURFACE_NAME_BYTES} bytes"
            ));
        }
        if self.node.is_some_and(|id| id == 0 || id > MAX_NODE_ID) {
            return Err("node ids are positive whole numbers".into());
        }
        if self.at_pointer && self.node.is_some() {
            return Err("choose either a node or the pointer".into());
        }
        if let Some(duration) = self.highlight {
            if !(MIN_HIGHLIGHT..=MAX_HIGHLIGHT).contains(&duration) {
                return Err("a highlight lasts between 0.1 and 60 seconds".into());
            }
            if !self.at_pointer && self.node.is_none() {
                return Err("a highlight needs a node or the pointer".into());
            }
            if *selection == OutputSelection::All {
                return Err("a highlight applies to one output, not all outputs".into());
            }
        }
        Ok(())
    }
}

/// one parsed inspection request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InspectRequest {
    pub(crate) selection: OutputSelection,
    pub(crate) query: InspectQuery,
}

pub(crate) fn encode_query(query: &InspectQuery) -> Json {
    let mut object = Map::new();
    if query.keys {
        object.insert("keys".into(), Json::Bool(true));
    }
    if query.at_pointer {
        object.insert("at_pointer".into(), Json::Bool(true));
    }
    if let Some(surface) = &query.surface {
        object.insert("surface".into(), Json::String(surface.clone()));
    }
    if let Some(node) = query.node {
        object.insert("node".into(), Json::from(node));
    }
    if let Some(duration) = query.highlight {
        object.insert("highlight".into(), Json::from(duration.as_secs_f64()));
    }
    Json::Object(object)
}

pub(crate) fn parse_query(
    value: Json,
    selection: &OutputSelection,
) -> Result<InspectQuery, String> {
    let Json::Object(mut object) = value else {
        return Err("inspect must be a json object".into());
    };
    if let Some(key) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "keys" | "at_pointer" | "surface" | "node" | "highlight"
        )
    }) {
        return Err(format!("unknown inspect field {key:?}"));
    }
    let flag = |value: Option<Json>, name: &str| match value {
        None | Some(Json::Bool(false)) => Ok(false),
        Some(Json::Bool(true)) => Ok(true),
        Some(_) => Err(format!("{name} must be a boolean")),
    };
    let query = InspectQuery {
        keys: flag(object.remove("keys"), "keys")?,
        at_pointer: flag(object.remove("at_pointer"), "at_pointer")?,
        surface: match object.remove("surface") {
            None | Some(Json::Null) => None,
            Some(Json::String(name)) => Some(name),
            Some(_) => return Err("surface must be a string".into()),
        },
        node: match object.remove("node") {
            None | Some(Json::Null) => None,
            Some(Json::Number(id)) => {
                Some(id.as_u64().ok_or("node ids are positive whole numbers")?)
            }
            Some(_) => return Err("node must be a number".into()),
        },
        highlight: match object.remove("highlight") {
            None | Some(Json::Null) => None,
            Some(Json::Number(seconds)) => Some(
                seconds
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0 && *seconds <= 60.0)
                    .map(Duration::from_secs_f64)
                    .ok_or("a highlight lasts between 0.1 and 60 seconds")?,
            ),
            Some(_) => return Err("highlight must be a number of seconds".into()),
        },
    };
    query.validate(selection)?;
    Ok(query)
}

/// one widget captured with the frame that painted it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeRecord {
    pub(crate) id: NodeId,
    pub(crate) kind: Kind,
    pub(crate) parent: Option<NodeId>,
    pub(crate) depth: u8,
    pub(crate) visible: bool,
    /// the key cut to `MAX_KEY_BYTES`, and whether it was cut.
    pub(crate) key: Option<(Box<str>, bool)>,
}

/// a presented frame and the widget metadata it was painted from.
pub(crate) struct FrameSnapshot {
    pub(crate) layout: Rc<Layout>,
    pub(crate) consumer: String,
    pub(crate) targets: BTreeMap<NodeId, UiTarget>,
    pub(crate) records: Rc<[NodeRecord]>,
    /// widgets past `MAX_CAPTURED_NODES` that were not captured.
    pub(crate) uncaptured: usize,
    pub(crate) size: (u32, u32),
    pub(crate) scale: f32,
    /// a popup's position in its parent when this frame presented.
    pub(crate) origin: Option<(f64, f64)>,
}

/// the compact widget metadata of one surface tree, in painting order. popups
/// are separate surfaces and are skipped.
pub(crate) fn capture(root: &Node) -> (Rc<[NodeRecord]>, usize) {
    fn visit(
        node: &Node,
        parent: Option<NodeId>,
        depth: u8,
        visible: bool,
        records: &mut Vec<NodeRecord>,
        uncaptured: &mut usize,
    ) {
        if records.len() >= MAX_CAPTURED_NODES {
            *uncaptured += 1;
        } else {
            records.push(NodeRecord {
                id: node.id,
                kind: node.kind,
                parent,
                depth,
                visible: visible && node.visible,
                key: node.key.as_deref().map(truncated_key),
            });
        }
        for child in node
            .children
            .iter()
            .filter(|child| child.kind != Kind::Popup)
        {
            visit(
                child,
                Some(node.id),
                depth.saturating_add(1),
                visible && node.visible,
                records,
                uncaptured,
            );
        }
    }
    let mut records = Vec::new();
    let mut uncaptured = 0;
    visit(root, None, 0, true, &mut records, &mut uncaptured);
    (records.into(), uncaptured)
}

fn truncated_key(key: &str) -> (Box<str>, bool) {
    if key.len() <= MAX_KEY_BYTES {
        return (key.into(), false);
    }
    let mut end = MAX_KEY_BYTES;
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    (key[..end].into(), true)
}

/// the reply's remaining room, shared by every output in one reply.
pub(crate) struct Budget {
    bytes: usize,
    nodes: usize,
}

impl Budget {
    pub(crate) fn new() -> Self {
        Self {
            bytes: MAX_REPLY_BYTES - REPLY_RESERVE,
            nodes: MAX_REPORT_NODES,
        }
    }

    /// charges one serialized part, or refuses it without charging.
    fn charge(&mut self, value: &Json) -> bool {
        self.charge_with_overhead(value, 1)
    }

    fn charge_with_overhead(&mut self, value: &Json, overhead: usize) -> bool {
        let size = serde_json::to_vec(value)
            .ok()
            .and_then(|bytes| bytes.len().checked_add(overhead))
            .unwrap_or(usize::MAX);
        if size > self.bytes {
            return false;
        }
        self.bytes -= size;
        true
    }

    pub(crate) fn charge_output(&mut self, name: Option<&str>) -> bool {
        // reserve the result wrapper and the bounded per-output diagnostic,
        // even when this output has no surfaces or widgets to charge.
        self.charge_with_overhead(
            &name.map_or(Json::Null, |name| Json::String(name.into())),
            128,
        )
    }
}

/// the surface one report section describes.
pub(crate) struct SurfaceView<'a> {
    pub(crate) name: &'a str,
    pub(crate) role: &'static str,
    pub(crate) root: NodeId,
    pub(crate) snapshot: &'a FrameSnapshot,
    /// the pointer's surface-local position and the widget painted under it.
    pub(crate) pointer: Option<(thing_core::Point, Option<NodeId>)>,
}

/// which captured widgets one surface's section lists.
pub(crate) enum Selection {
    All,
    Subtree(NodeId),
    /// the picked widget and its ancestors, outermost first.
    Path(NodeId),
}

/// builds one surface's section, charging every part to `budget` before it is
/// added. `None` means not even the section header fit.
pub(crate) fn describe_surface(
    view: &SurfaceView<'_>,
    selection: &Selection,
    keys: bool,
    budget: &mut Budget,
) -> Option<Json> {
    let snapshot = view.snapshot;
    let mut header = Map::new();
    if keys {
        let (name, truncated) = truncated_key(view.name);
        header.insert("surface".into(), Json::String(name.into()));
        if truncated {
            header.insert("surface_truncated".into(), Json::Bool(true));
        }
    }
    header.insert("role".into(), Json::String(view.role.into()));
    header.insert("root".into(), Json::from(view.root.0));
    header.insert(
        "size".into(),
        Json::Array(vec![snapshot.size.0.into(), snapshot.size.1.into()]),
    );
    header.insert("scale".into(), number(f64::from(snapshot.scale)));
    if let Some((x, y)) = snapshot.origin {
        header.insert("origin".into(), Json::Array(vec![number(x), number(y)]));
    }
    if let Some((point, picked)) = view.pointer {
        header.insert(
            "pointer".into(),
            Json::Array(vec![number(f64::from(point.x)), number(f64::from(point.y))]),
        );
        header.insert(
            "picked".into(),
            picked.map_or(Json::Null, |id| Json::from(id.0)),
        );
    }
    let header = Json::Object(header);
    // nodes and omission counts are added after the charged header.
    if !budget.charge_with_overhead(&header, 64) {
        return None;
    }
    let Json::Object(mut section) = header else {
        unreachable!("the header is an object");
    };
    let records: Vec<&NodeRecord> = match selection {
        Selection::All => snapshot.records.iter().collect(),
        Selection::Subtree(id) => subtree(&snapshot.records, *id),
        Selection::Path(id) => path(&snapshot.records, *id),
    };
    let mut nodes = Vec::new();
    let mut omitted = 0_usize;
    for record in &records {
        if budget.nodes == 0 {
            omitted += 1;
            continue;
        }
        let entry = describe_node(record, &snapshot.layout, keys);
        if budget.charge(&entry) {
            budget.nodes -= 1;
            nodes.push(entry);
        } else {
            omitted += 1;
        }
    }
    section.insert("nodes".into(), Json::Array(nodes));
    let uncaptured = match selection {
        Selection::All => snapshot.uncaptured,
        Selection::Subtree(_) | Selection::Path(_) => 0,
    };
    if omitted + uncaptured > 0 {
        section.insert("omitted".into(), Json::from(omitted + uncaptured));
    }
    Some(Json::Object(section))
}

fn subtree(records: &[NodeRecord], id: NodeId) -> Vec<&NodeRecord> {
    let Some(start) = records.iter().position(|record| record.id == id) else {
        return Vec::new();
    };
    let depth = records[start].depth;
    std::iter::once(&records[start])
        .chain(
            records[start + 1..]
                .iter()
                .take_while(|record| record.depth > depth),
        )
        .collect()
}

fn path(records: &[NodeRecord], id: NodeId) -> Vec<&NodeRecord> {
    let mut path = Vec::new();
    let mut next = Some(id);
    while let Some(id) = next {
        let Some(record) = records.iter().find(|record| record.id == id) else {
            break;
        };
        path.push(record);
        next = record.parent;
    }
    path.reverse();
    path
}

fn describe_node(record: &NodeRecord, layout: &Layout, keys: bool) -> Json {
    let mut entry = Map::new();
    entry.insert("id".into(), Json::from(record.id.0));
    entry.insert("kind".into(), Json::String(kind_name(record.kind).into()));
    entry.insert(
        "parent".into(),
        record.parent.map_or(Json::Null, |id| Json::from(id.0)),
    );
    entry.insert("visible".into(), Json::Bool(record.visible));
    if keys && let Some((key, cut)) = &record.key {
        entry.insert("key".into(), Json::String(key.to_string()));
        if *cut {
            entry.insert("key_truncated".into(), Json::Bool(true));
        }
    }
    match layout.semantic_placement(record.id) {
        Some(semantic) => {
            entry.insert("bounds".into(), rect(semantic.bounds));
            entry.insert("visual".into(), rect(semantic.border_aabb));
            entry.insert("enabled".into(), Json::Bool(semantic.enabled));
        }
        None => {
            entry.insert("bounds".into(), Json::Null);
        }
    }
    match layout.get(record.id) {
        Some(placement) => {
            entry.insert("clip".into(), rect(placement.clip));
            entry.insert("opacity".into(), number(f64::from(placement.opacity)));
            entry.insert("interactive".into(), Json::Bool(placement.interactive));
        }
        // laid out, but clipped away or hidden, so nothing paints.
        None => {
            entry.insert("clip".into(), Json::Null);
        }
    }
    Json::Object(entry)
}

fn rect(rect: Rect) -> Json {
    Json::Array(vec![
        number(f64::from(rect.x)),
        number(f64::from(rect.y)),
        number(f64::from(rect.width)),
        number(f64::from(rect.height)),
    ])
}

// two decimals keep reports compact and stable across tiny float noise.
fn number(value: f64) -> Json {
    let rounded = (value * 100.0).round() / 100.0;
    serde_json::Number::from_f64(if rounded == 0.0 { 0.0 } else { rounded })
        .map_or(Json::Null, Json::Number)
}

pub(crate) fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Panel => "panel",
        Kind::Window => "window",
        Kind::Lock => "lock",
        Kind::Popup => "popup",
        Kind::Row => "row",
        Kind::CenterBox => "centerbox",
        Kind::Column => "column",
        Kind::Stack => "stack",
        Kind::Scroll => "scroll",
        Kind::Text => "text",
        Kind::TextInput => "text_input",
        Kind::SecretInput => "secret_input",
        Kind::Image => "image",
        Kind::Path => "path",
        Kind::Button => "button",
        Kind::PointerArea => "pointer_area",
        Kind::Slider => "slider",
        Kind::Range => "range",
    }
}

/// the outcome for one output: its report, or why there is none.
pub(crate) struct OutputReport {
    pub(crate) output: Option<String>,
    pub(crate) result: Result<Json, String>,
}

/// the reply to one inspection request.
pub(crate) enum InspectReply {
    Failed(String),
    Reported(Vec<OutputReport>),
}

pub(crate) fn encode_reply(reply: &InspectReply) -> Vec<u8> {
    let json = match reply {
        InspectReply::Failed(error) => serde_json::json!({ "ok": false, "error": error }),
        InspectReply::Reported(reports) => {
            let ok = reports.iter().all(|report| report.result.is_ok());
            let results: Vec<_> = reports
                .iter()
                .map(|report| {
                    let mut object = Map::new();
                    object.insert(
                        "output".into(),
                        report.output.clone().map_or(Json::Null, Json::String),
                    );
                    match &report.result {
                        Ok(value) => object.insert("value".into(), value.clone()),
                        Err(error) => object.insert("error".into(), Json::String(error.clone())),
                    };
                    Json::Object(object)
                })
                .collect();
            serde_json::json!({ "ok": ok, "results": results })
        }
    };
    let mut bytes = serde_json::to_vec(&json).expect("json values serialize");
    // the budget keeps reports below the limit; this only guards mistakes.
    if bytes.len() >= MAX_REPLY_BYTES {
        bytes = serde_json::to_vec(&serde_json::json!({
            "ok": false,
            "error": "the inspection report exceeds the reply limit",
        }))
        .expect("json values serialize");
    }
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
#[path = "inspection_tests.rs"]
mod tests;
