//! the running host's side of live inspection: frame snapshots, reports, and
//! the one timed highlight. nothing here grabs input, changes focus or
//! layout, or runs configuration code.

use std::time::Instant;

use serde_json::{Map, Value as Json};
use thing_core::NodeId;
use thing_runtime::SurfaceKey;
use thing_ui::InspectorHighlight;

use super::{App, Role, Surface, SurfaceOwner};
use crate::{
    focus::ScopedUiTarget,
    inspection::{
        Budget, FrameSnapshot, InspectReply, InspectRequest, OutputReport, Selection, SurfaceView,
        describe_surface,
    },
};

/// live inspection state. the host has one only with `thing run --inspector`.
#[derive(Default)]
pub(crate) struct Inspector {
    highlight: Option<Highlight>,
}

/// a widget highlighted until `until`, bound to its scoped identity like
/// presented input targets.
#[derive(Clone)]
struct Highlight {
    owner: SurfaceOwner,
    root: NodeId,
    target: ScopedUiTarget,
    until: Instant,
}

impl Inspector {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.highlight.as_ref().map(|highlight| highlight.until)
    }
}

fn surface_name(key: &SurfaceKey) -> &str {
    match key {
        SurfaceKey::Default => "default",
        SurfaceKey::Named(name) => name,
    }
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::Panel(_) => "panel",
        Role::Window(_) => "window",
        Role::Popup(_) => "popup",
    }
}

impl App {
    /// ends the highlight and redraws the surface that showed it.
    pub(super) fn clear_highlight(&mut self) {
        let Some(highlight) = self
            .inspector
            .as_mut()
            .and_then(|inspector| inspector.highlight.take())
        else {
            return;
        };
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.owner() == highlight.owner && surface.root == highlight.root)
        {
            surface.dirty = true;
        }
    }

    /// ends a highlight that expired, whose configuration retired, or whose
    /// surface or widget is gone or hidden.
    pub(super) fn reconcile_highlight(&mut self, now: Instant) {
        let Some(highlight) = self
            .inspector
            .as_ref()
            .and_then(|inspector| inspector.highlight.as_ref())
        else {
            return;
        };
        let output = &highlight.owner.output;
        let stale = highlight.until <= now
            || self.configurations.consumer(output) != Some(highlight.target.consumer.as_str())
            || !self
                .configurations
                .ui_target_identity_is_current(output, &highlight.target.target)
            || !self.surfaces.iter().any(|surface| {
                surface.owner() == highlight.owner && surface.root == highlight.root
            });
        if stale {
            self.clear_highlight();
        }
    }

    pub(super) fn inspect_request(&mut self, request: InspectRequest) -> InspectReply {
        if self.inspector.is_none() {
            return InspectReply::Failed(crate::command::INSPECTOR_DISABLED.into());
        }
        let outputs = match self.configurations.output_targets(&request.selection) {
            Ok(outputs) => outputs,
            Err(error) => return InspectReply::Failed(error),
        };
        let query = &request.query;
        let mut budget = Budget::new();
        let mut reports = Vec::new();
        let mut omitted_outputs = 0;
        let mut picked_target = None;
        for output in outputs {
            let name = self
                .configurations
                .output(&output)
                .and_then(|context| context.name.clone());
            if !budget.charge_output(name.as_deref()) {
                omitted_outputs += 1;
                continue;
            }
            let mut sections = Vec::new();
            let mut omitted_surfaces = 0;
            let mut found_node = false;
            let mut found_surface = false;
            for surface in self
                .surfaces
                .iter()
                .filter(|surface| surface.output == output)
            {
                if query
                    .surface
                    .as_deref()
                    .is_some_and(|name| name != surface_name(&surface.key))
                {
                    continue;
                }
                // a surface that has not presented yet has nothing to show.
                let Some(snapshot) = &surface.inspection else {
                    continue;
                };
                found_surface = true;
                let (selection, pointer) = if query.at_pointer {
                    let Some(point) = surface.pointer_position else {
                        continue;
                    };
                    let picked = snapshot.layout.paint_target(point);
                    if let Some(id) = picked {
                        picked_target = Some((surface.owner(), surface.root, id));
                    }
                    (
                        picked.map_or(Selection::All, Selection::Path),
                        Some((point, picked)),
                    )
                } else if let Some(id) = query.node {
                    let id = NodeId(id);
                    if !snapshot.records.iter().any(|record| record.id == id) {
                        continue;
                    }
                    found_node = true;
                    picked_target = Some((surface.owner(), surface.root, id));
                    (Selection::Subtree(id), None)
                } else {
                    (Selection::All, None)
                };
                let view = SurfaceView {
                    name: surface_name(&surface.key),
                    role: role_name(&surface.role),
                    root: surface.root,
                    snapshot,
                    pointer,
                };
                match describe_surface(&view, &selection, query.keys, &mut budget) {
                    Some(section) => sections.push(section),
                    None => omitted_surfaces += 1,
                }
            }
            let result = if query.surface.is_some() && !found_surface {
                Err("no presented surface matches --surface".into())
            } else if query.node.is_some() && !found_node {
                Err(format!(
                    "no presented surface shows node {}",
                    query.node.unwrap_or_default()
                ))
            } else if query.at_pointer && sections.is_empty() && omitted_surfaces == 0 {
                Err("the pointer is not over a presented surface".into())
            } else {
                let mut value = Map::new();
                value.insert("surfaces".into(), Json::Array(sections));
                if omitted_surfaces > 0 {
                    value.insert("omitted_surfaces".into(), Json::from(omitted_surfaces));
                }
                Ok(Json::Object(value))
            };
            reports.push(OutputReport {
                output: name,
                result,
            });
        }
        if let Some(duration) = query.highlight {
            match picked_target {
                Some((owner, root, id)) => {
                    if let Err(error) = self.start_highlight(owner, root, id, duration) {
                        return InspectReply::Failed(error);
                    }
                }
                None if reports.iter().all(|report| report.result.is_err()) => {}
                None => {
                    return InspectReply::Failed("no widget to highlight".into());
                }
            }
        }
        if omitted_outputs > 0 {
            reports.push(OutputReport {
                output: None,
                result: Err(format!(
                    "{omitted_outputs} output(s) omitted; the report reached its size limit"
                )),
            });
        }
        InspectReply::Reported(reports)
    }

    fn start_highlight(
        &mut self,
        owner: SurfaceOwner,
        root: NodeId,
        id: NodeId,
        duration: std::time::Duration,
    ) -> Result<(), String> {
        let snapshot = self
            .surfaces
            .iter()
            .find(|surface| surface.owner() == owner && surface.root == root)
            .and_then(|surface| surface.inspection.as_ref())
            .ok_or("the surface has no submitted frame")?;
        let target = snapshot
            .targets
            .get(&id)
            .filter(|target| {
                self.configurations.consumer(&owner.output) == Some(snapshot.consumer.as_str())
                    && self
                        .configurations
                        .ui_target_identity_is_current(&owner.output, target)
                    && self.configurations.rendered_surface_root(&owner.output, id) == Some(root)
                    && snapshot.layout.semantic_placement(id).is_some()
            })
            .cloned()
            .ok_or_else(|| format!("node {} no longer matches the inspected frame", id.0))?;
        let consumer = snapshot.consumer.clone();
        // one highlight per instance: the previous one ends and redraws.
        self.clear_highlight();
        if let Some(inspector) = &mut self.inspector {
            inspector.highlight = Some(Highlight {
                owner: owner.clone(),
                root,
                target: ScopedUiTarget { consumer, target },
                until: Instant::now() + duration,
            });
        }
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.owner() == owner && surface.root == root)
        {
            surface.dirty = true;
        }
        Ok(())
    }
}

/// what a frame of `surface` paints for the inspector, from the frame's own
/// layout. `Err(())` means the highlighted widget is gone from this surface,
/// so the highlight should end.
pub(super) fn highlight_for(
    inspector: Option<&Inspector>,
    configurations: &crate::configuration::Configurations<
        wayland_client::protocol::wl_output::WlOutput,
    >,
    surface: &Surface,
    layout: &thing_ui::Layout,
) -> Result<Option<InspectorHighlight>, ()> {
    let Some(highlight) = inspector.and_then(|inspector| inspector.highlight.as_ref()) else {
        return Ok(None);
    };
    if highlight.owner != surface.owner() || highlight.root != surface.root {
        return Ok(None);
    }
    let node = highlight.target.target.node();
    let current = configurations.consumer(&surface.output)
        == Some(highlight.target.consumer.as_str())
        && configurations.ui_target_identity_is_current(&surface.output, &highlight.target.target)
        && layout.semantic_placement(node).is_some();
    if current {
        Ok(Some(InspectorHighlight { node }))
    } else {
        Err(())
    }
}

/// the snapshot a frame keeps once it is presented.
pub(super) fn snapshot(
    root: &thing_core::Node,
    layout: std::rc::Rc<thing_ui::Layout>,
    size: (u32, u32),
    scale: f32,
    origin: Option<(f64, f64)>,
    runtime: &thing_runtime::Runtime,
    consumer: String,
) -> FrameSnapshot {
    let (records, uncaptured) = crate::inspection::capture(root);
    let ids = records.iter().map(|record| record.id).collect();
    FrameSnapshot {
        layout,
        consumer,
        targets: runtime.ui_targets(&ids),
        records,
        uncaptured,
        size,
        scale,
        origin,
    }
}

#[cfg(test)]
#[path = "inspection_tests.rs"]
mod tests;
