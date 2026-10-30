use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use mlua::{Error, Lua, Result, Table};
use thing_core::{Kind, NodeId};

use crate::UiContext;

/// a border box in logical surface coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoundsRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl BoundsRect {
    fn validate(self) -> Result<()> {
        if ![
            self.x,
            self.y,
            self.width,
            self.height,
            self.x + self.width,
            self.y + self.height,
        ]
        .into_iter()
        .all(f64::is_finite)
            || self.width < 0.0
            || self.height < 0.0
        {
            return Err(Error::runtime(
                "presented bounds must be finite with nonnegative sizes",
            ));
        }
        Ok(())
    }

    fn into_lua(self, lua: &Lua) -> Result<Table> {
        let result = lua.create_table()?;
        result.raw_set("x", self.x)?;
        result.raw_set("y", self.y)?;
        result.raw_set("width", self.width)?;
        result.raw_set("height", self.height)?;
        result.set_readonly(true);
        Ok(result)
    }
}

/// stable layout and transformed visual boxes from one presented frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WidgetBounds {
    pub node: NodeId,
    pub layout: BoundsRect,
    pub visual: BoundsRect,
}

impl WidgetBounds {
    fn validate(self, node: NodeId) -> Result<()> {
        if self.node != node {
            return Err(Error::runtime(
                "bounds provider returned a different widget",
            ));
        }
        self.layout.validate()?;
        self.visual.validate()
    }
}

/// geometry retained for one submitted frame. implementations must keep these
/// measurements unchanged for the lifetime of the frame lease.
pub trait BoundsProvider {
    fn get(&self, node: NodeId) -> Option<WidgetBounds>;
}

/// a frame lease: dropping the last owner retires its luau measurements.
pub struct PresentedBounds {
    root: NodeId,
    provider: Rc<dyn BoundsProvider>,
}

#[derive(Default)]
pub(crate) struct Bounds {
    frames: BTreeMap<NodeId, Weak<PresentedBounds>>,
    allowed: bool,
}

impl Bounds {
    pub(crate) fn install(
        owner: &Rc<RefCell<Self>>,
        context: &Rc<RefCell<UiContext>>,
        lua: &Lua,
        ui: &Table,
    ) -> Result<()> {
        let owner = Rc::downgrade(owner);
        let context = Rc::downgrade(context);
        ui.set(
            "bounds",
            lua.create_function(move |lua, widget: Table| {
                let owner = owner
                    .upgrade()
                    .ok_or_else(|| Error::runtime("bounds runtime has ended"))?;
                let (node, frame) = {
                    let bounds = owner.borrow();
                    if !bounds.allowed {
                        return Err(Error::runtime(
                            "ui.bounds is available only inside event handlers",
                        ));
                    }
                    let source = widget
                        .raw_get::<u64>("_id")
                        .map_err(|_| Error::runtime("ui.bounds target must be a widget"))?;
                    let context = context
                        .upgrade()
                        .ok_or_else(|| Error::runtime("bounds runtime has ended"))?;
                    let context = context.borrow();
                    let Some(target) = context.targets.get(&source) else {
                        return Ok(None);
                    };
                    let root = target
                        .lineage
                        .iter()
                        .rev()
                        .find(|identity| identity.kind == Kind::Popup)
                        .or_else(|| target.lineage.first())
                        .expect("widget targets have a surface root")
                        .node;
                    let Some(frame) = bounds.frames.get(&root).and_then(Weak::upgrade) else {
                        return Ok(None);
                    };
                    (target.node, frame)
                };
                let Some(widget) = frame.provider.get(node) else {
                    return Ok(None);
                };
                widget.validate(node)?;
                let result = lua.create_table()?;
                result.raw_set("surface", frame.root.0.to_string())?;
                result.raw_set("layout", widget.layout.into_lua(lua)?)?;
                result.raw_set("visual", widget.visual.into_lua(lua)?)?;
                result.set_readonly(true);
                Ok(Some(result))
            })?,
        )
    }

    pub(crate) fn allow_reads(&mut self, allowed: bool) {
        self.allowed = allowed;
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }

    pub(crate) fn record(
        &mut self,
        root: NodeId,
        provider: Rc<dyn BoundsProvider>,
    ) -> Result<Rc<PresentedBounds>> {
        provider
            .get(root)
            .ok_or_else(|| Error::runtime("presented bounds must include their surface root"))?
            .validate(root)?;
        self.frames.retain(|_, frame| frame.strong_count() != 0);
        let frame = Rc::new(PresentedBounds { root, provider });
        self.frames.insert(root, Rc::downgrade(&frame));
        Ok(frame)
    }
}
