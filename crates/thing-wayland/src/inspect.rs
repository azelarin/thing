use std::path::Path;

use thing_core::{Kind, Node, Rect, Scale};
use thing_runtime::{OutputContext, Runtime, ServiceSnapshot, SurfaceKey};
use thing_ui::{Layout, Size, TextSystem};

use crate::{panel, popup};

/// evaluates layout with an explicit output context and no desktop connections.
pub struct LayoutInspector {
    output: OutputContext,
}

impl LayoutInspector {
    pub fn new(size: (u32, u32), name: Option<String>, scale_120ths: u32) -> Result<Self, String> {
        if size.0 == 0 || size.1 == 0 || size.0 > 100_000 || size.1 > 100_000 {
            return Err("output dimensions must be between 1 and 100,000 logical pixels".into());
        }
        if name
            .as_ref()
            .is_some_and(|name| name.len() > 4096 || name.contains('\0'))
        {
            return Err("output name must be at most 4096 bytes without NUL".into());
        }
        let scale = Scale::from_120ths(scale_120ths)
            .ok_or("output scale must be between 1/120 and 16 in steps of 1/120")?;
        Ok(Self {
            output: OutputContext {
                name,
                width: size.0,
                height: size.1,
                scale,
            },
        })
    }

    /// returns an initial layout snapshot using the renderer's text measurer.
    /// services use their default state; timers and processes remain unarmed.
    pub fn inspect(&self, entry: &Path) -> Result<String, String> {
        let runtime =
            Runtime::load_for_output(entry, self.output.clone(), &ServiceSnapshot::default())
                .map_err(|error| error.to_string())?;
        let mut report = format!(
            "output {} size={}x{} scale={}\n",
            optional_name(self.output.name.as_deref()),
            self.output.width,
            self.output.height,
            self.output.scale.factor()
        );
        let mut text = TextSystem::default();
        for surface in runtime.surfaces() {
            let key = match &surface.key {
                SurfaceKey::Default => "default".into(),
                SurfaceKey::Named(key) => format!("{key:?}"),
            };
            let size = panel::logical_size(
                panel::geometry(&surface.root, None),
                (self.output.width, self.output.height),
            );
            describe_surface(
                &mut report,
                &format!("panel {key}"),
                &surface.root,
                size,
                &mut text,
            )?;
            describe_popups(&mut report, &surface.root, &mut text)?;
        }
        Ok(report)
    }
}

fn describe_surface(
    report: &mut String,
    label: &str,
    root: &Node,
    size: (u32, u32),
    text: &mut TextSystem,
) -> Result<(), String> {
    let layout = Layout::compute(
        root,
        Size {
            width: size.0 as f32,
            height: size.1 as f32,
        },
        text,
    )?;
    report.push_str(&format!("{label} size={}x{}\n", size.0, size.1));
    describe_node(report, root, &layout, 0, false);
    Ok(())
}

fn describe_node(report: &mut String, node: &Node, layout: &Layout, depth: usize, hidden: bool) {
    let hidden = hidden || !node.visible;
    let semantic = layout.semantic_placement(node.id);
    let placed = layout.get(node.id);
    report.push_str(&format!(
        "{}{}#{} key={} visible={}",
        "  ".repeat(depth),
        kind(node.kind),
        node.id.0,
        optional_name(node.key.as_deref()),
        !hidden,
    ));
    if let Some(semantic) = semantic {
        report.push_str(&format!(
            " bounds={} visual={} enabled={}",
            rect(semantic.bounds),
            rect(semantic.border_aabb),
            semantic.enabled,
        ));
    } else {
        report.push_str(" bounds=none");
    }
    if let Some(placed) = placed {
        report.push_str(&format!(
            " clip={} opacity={:.3}",
            rect(placed.clip),
            placed.opacity
        ));
    } else {
        report.push_str(" body=omitted");
    }
    report.push('\n');
    for child in &node.children {
        if child.kind != Kind::Popup {
            describe_node(report, child, layout, depth + 1, hidden);
        }
    }
}

fn describe_popups(report: &mut String, node: &Node, text: &mut TextSystem) -> Result<(), String> {
    for child in &node.children {
        if child.kind == Kind::Popup {
            describe_surface(
                report,
                &format!(
                    "popup#{} key={} (requested layout)",
                    child.id.0,
                    optional_name(child.key.as_deref())
                ),
                child,
                popup::logical_size(child),
                text,
            )?;
        }
        describe_popups(report, child, text)?;
    }
    Ok(())
}

fn rect(rect: Rect) -> String {
    format!(
        "[{:.2},{:.2} {:.2}x{:.2}]",
        rect.x, rect.y, rect.width, rect.height
    )
}

fn optional_name(name: Option<&str>) -> String {
    name.map_or_else(|| "-".into(), |name| format!("{name:?}"))
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Panel => "panel",
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
