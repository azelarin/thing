use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use mlua::{Error, Lua, Result, Table, Value};
use thing_core::{Kind, NodeId};

use crate::{UiContext, UiTarget};

/// the root of the surface a target is drawn on: its nearest popup, or the
/// root of its tree.
pub(crate) fn surface_root(target: &UiTarget) -> NodeId {
    *surface_chain(target).last().expect("chains have a root")
}

/// the surfaces from a target's tree root down to the one it is drawn on.
fn surface_chain(target: &UiTarget) -> Vec<NodeId> {
    let mut chain = vec![
        target
            .lineage
            .first()
            .expect("widget targets have a surface root")
            .node,
    ];
    chain.extend(
        target
            .lineage
            .iter()
            .skip(1)
            .filter(|identity| identity.kind == Kind::Popup)
            .map(|identity| identity.node),
    );
    chain
}

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
    pub(crate) fn validate(self, node: NodeId) -> Result<()> {
        if self.node != node {
            return Err(Error::runtime(
                "bounds provider returned a different widget",
            ));
        }
        self.layout.validate()?;
        self.visual.validate()
    }
}

/// a point mapped between a widget's own box and its surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MappedPoint {
    Point {
        x: f64,
        y: f64,
    },
    /// the frame has no placement for the widget.
    Unavailable,
    /// the widget's transform collapses its box, so surface points have no
    /// local position.
    Noninvertible,
}

/// geometry retained for one submitted frame. implementations must keep these
/// measurements unchanged for the lifetime of the frame lease.
pub trait BoundsProvider {
    fn get(&self, node: NodeId) -> Option<WidgetBounds>;

    /// the width and height a widget asks for with unlimited space, measured
    /// by the frame's text and layout engines, when layout measured it.
    fn content(&self, _node: NodeId) -> Option<(f64, f64)> {
        None
    }

    /// maps a point in the widget's untransformed border box, with its origin
    /// at the top left, to surface coordinates.
    fn local_to_surface(&self, _node: NodeId, _x: f64, _y: f64) -> MappedPoint {
        MappedPoint::Unavailable
    }

    /// maps a surface point into the widget's untransformed border box.
    fn surface_to_local(&self, _node: NodeId, _x: f64, _y: f64) -> MappedPoint {
        MappedPoint::Unavailable
    }

    /// for a popup, where its surface origin sat in its parent surface when
    /// this frame was presented.
    fn origin(&self) -> Option<(f64, f64)> {
        None
    }

    /// whether the surface's size or scroll offsets changed since its
    /// previous frame. geometry that moved for those reasons did not feed
    /// back into its own layout.
    fn moved(&self) -> bool {
        false
    }
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
        owner_strong: &Rc<RefCell<Self>>,
        context_strong: &Rc<RefCell<UiContext>>,
        lua: &Lua,
        ui: &Table,
    ) -> Result<()> {
        let owner = Rc::downgrade(owner_strong);
        let context = Rc::downgrade(context_strong);
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
                    let root = surface_root(target);
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
        )?;
        let owner = Rc::downgrade(owner_strong);
        let context = Rc::downgrade(context_strong);
        ui.set(
            "convert",
            lua.create_function(
                move |lua, (from, x, y, to): (Value, f64, f64, Value)| -> Result<Option<Table>> {
                    let owner = owner
                        .upgrade()
                        .ok_or_else(|| Error::runtime("bounds runtime has ended"))?;
                    let context = context
                        .upgrade()
                        .ok_or_else(|| Error::runtime("bounds runtime has ended"))?;
                    let bounds = owner.borrow();
                    if !bounds.allowed {
                        return Err(Error::runtime(
                            "ui.convert is available only inside event handlers",
                        ));
                    }
                    if !x.is_finite() || !y.is_finite() {
                        return Err(Error::runtime("ui.convert point must be finite"));
                    }
                    let source = |widget: &Value| {
                        widget
                            .as_table()
                            .and_then(|widget| widget.raw_get::<u64>("_id").ok())
                            .ok_or_else(|| Error::runtime("ui.convert targets must be widgets"))
                    };
                    let (from, to) = (source(&from)?, source(&to)?);
                    let context = context.borrow();
                    let (Some(from), Some(to)) =
                        (context.targets.get(&from), context.targets.get(&to))
                    else {
                        return Ok(None);
                    };
                    let Some(point) = bounds.convert(from, (x, y), to)? else {
                        return Ok(None);
                    };
                    let result = lua.create_table()?;
                    result.raw_set("x", point.0)?;
                    result.raw_set("y", point.1)?;
                    result.set_readonly(true);
                    Ok(Some(result))
                },
            )?,
        )
    }

    /// maps a point from one widget's box to another's through the frames
    /// last presented for their surfaces. widgets in different trees share no
    /// coordinate space; popups relate through the origins their parents
    /// placed them at.
    fn convert(
        &self,
        from: &UiTarget,
        point: (f64, f64),
        to: &UiTarget,
    ) -> Result<Option<(f64, f64)>> {
        let from_chain = surface_chain(from);
        let to_chain = surface_chain(to);
        if from_chain[0] != to_chain[0] {
            return Err(Error::runtime(
                "ui.convert widgets are on unrelated surfaces",
            ));
        }
        let common = from_chain
            .iter()
            .zip(&to_chain)
            .take_while(|(from, to)| from == to)
            .count();
        let frame = |root: &NodeId| self.frames.get(root).and_then(Weak::upgrade);
        let finite = |(x, y): (f64, f64)| {
            if x.is_finite() && y.is_finite() {
                Ok((x, y))
            } else {
                Err(Error::runtime("presented bounds must be finite"))
            }
        };
        let Some(source) = frame(from_chain.last().expect("chains have a root")) else {
            return Ok(None);
        };
        let (mut x, mut y) = match source
            .provider
            .local_to_surface(from.node, point.0, point.1)
        {
            MappedPoint::Point { x, y } => finite((x, y))?,
            MappedPoint::Unavailable => return Ok(None),
            MappedPoint::Noninvertible => {
                return Err(Error::runtime(
                    "ui.convert source transform is not invertible",
                ));
            }
        };
        // up from the source surface to the shared one, then down to the target.
        for root in from_chain[common..].iter().rev() {
            let Some(origin) = frame(root).and_then(|frame| frame.provider.origin()) else {
                return Ok(None);
            };
            (x, y) = finite((x + origin.0, y + origin.1))?;
        }
        for root in &to_chain[common..] {
            let Some(origin) = frame(root).and_then(|frame| frame.provider.origin()) else {
                return Ok(None);
            };
            (x, y) = finite((x - origin.0, y - origin.1))?;
        }
        let Some(target) = frame(to_chain.last().expect("chains have a root")) else {
            return Ok(None);
        };
        match target.provider.surface_to_local(to.node, x, y) {
            MappedPoint::Point { x, y } => finite((x, y)).map(Some),
            MappedPoint::Unavailable => Ok(None),
            MappedPoint::Noninvertible => Err(Error::runtime(
                "ui.convert target transform is not invertible",
            )),
        }
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
