//! reactive geometry handles.
//!
//! a handle names one widget identity. after the host presents a frame, it
//! publishes that frame's measurements; a handle whose measurement changed
//! bumps one revision signal, and property functions that read the handle
//! refresh on the next turn. reads never trigger layout themselves.
//!
//! luau owns handles: the handle table and each of its methods keep the
//! handle alive, and a handle nothing references is reclaimed, so its slot
//! and revision signal serve later handles. at most `MAX_HANDLES` may be
//! reachable at once.
//!
//! a measurement that feeds its own layout can loop. a publication that
//! changes a handle continues its surface's feedback chain when the accepted
//! tree changed since the surface's previous publication and nothing outside
//! geometry did: no state write, service update, or animation step that
//! geometry did not steer. geometry that moved without a new tree, after a
//! resize or scroll, starts a new chain, and an unchanged publication leaves
//! the chain as it is. after `MAX_FEEDBACK_ROUNDS` rounds the changing
//! handles keep their last value until something outside geometry changes.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use mlua::{Error, Lua, Result, Table, Value};
use thing_core::NodeId;

use crate::{
    UiContext, UiTarget,
    bounds::{BoundsProvider, BoundsRect, surface_root},
    state::{Scalar, Tracker},
    ui_target_identity_matches,
};

/// the most handles luau may reference at the same time.
pub(crate) const MAX_HANDLES: usize = 64;
/// quiet publications in a row that may still change geometry.
pub const MAX_FEEDBACK_ROUNDS: u32 = 8;

/// one widget's measurements from one presented frame.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Snapshot {
    surface: NodeId,
    layout: BoundsRect,
    visual: BoundsRect,
    content: Option<(f64, f64)>,
}

struct Handle {
    source: u64,
    /// the identity first resolved. it follows keyed re-creation and retires
    /// when the identity is gone, so a later widget never inherits it.
    target: Option<UiTarget>,
    retired: bool,
    revision: usize,
    /// what luau reads.
    snapshot: Option<Snapshot>,
    /// what the latest frame of the handle's surface measured. it differs
    /// from `snapshot` while the handle is frozen.
    measured: Option<Snapshot>,
    frozen: bool,
}

impl Handle {
    fn clear(&mut self) {
        self.snapshot = None;
        self.measured = None;
        self.frozen = false;
    }
}

/// shared by the registry and every luau function of one handle. the
/// registry's reference is the only one left once luau dropped the handle.
type Shared = Rc<RefCell<Handle>>;

/// what one publication changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GeometryUpdate {
    pub changed: bool,
    /// widgets whose geometry stopped publishing because it kept feeding back
    /// into its own layout.
    pub frozen: Vec<NodeId>,
}

/// counters a surface's chain compares between its publications.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Witness {
    /// tracker changes made outside geometry.
    outside: u64,
    /// animation frames that geometry did not steer.
    advances: u64,
    /// accepted trees that differed from the one before.
    commits: u64,
}

#[derive(Clone, Copy, Default)]
struct Chain {
    seen: Option<Witness>,
    /// changing publications in a row that only geometry caused.
    rounds: u32,
}

/// where a handle's identity is in the accepted tree.
enum Lookup {
    Current(UiTarget),
    /// the resolved identity left the tree; the handle retires.
    Gone,
    /// the widget has not been placed in the tree yet.
    Waiting,
}

#[derive(Default)]
pub(crate) struct Geometry {
    handles: Vec<Shared>,
    /// revision signals of reclaimed handles, reused before new ones.
    spare: Vec<usize>,
    chains: BTreeMap<NodeId, Chain>,
    /// tracker changes made by geometry itself.
    own_changes: u64,
    /// animation frames that changed the ui and were not caused by geometry.
    pub(crate) advances: u64,
    /// refreshes that changed the accepted tree.
    pub(crate) commits: u64,
    /// the last refresh that geometry alone caused changed where the ui is
    /// heading, so the animation frames that follow belong to the feedback.
    pub(crate) driven: bool,
    /// outside changes seen by the last refresh that evaluated properties.
    refresh_outside: Option<u64>,
    /// a handle was created, retired, or reclaimed, so measurement flags are
    /// stale.
    pub(crate) marks_stale: bool,
    /// whether the accepted tree may carry measurement flags.
    pub(crate) marked: bool,
    /// revisions changed outside a refresh, so dependents need one.
    pub(crate) refresh_pending: bool,
}

/// current identities by node, built once per pass over the handles.
struct Targets<'a>(BTreeMap<NodeId, &'a UiTarget>);

impl<'a> Targets<'a> {
    fn new(context: &'a UiContext) -> Self {
        let mut targets = BTreeMap::new();
        for target in context.targets.values() {
            targets.entry(target.node()).or_insert(target);
        }
        Self(targets)
    }
}

impl Geometry {
    /// whether layout needs to look at measurement flags at all.
    pub(crate) fn wants_marks(&self) -> bool {
        !self.handles.is_empty() || self.marked
    }

    /// tracker changes made by anything except geometry.
    pub(crate) fn outside(&self, tracker: &Tracker) -> u64 {
        tracker.changes - self.own_changes
    }

    /// whether only geometry changed state since the last refresh that
    /// evaluated properties.
    pub(crate) fn only_geometry_since_refresh(&self, tracker: &Tracker) -> bool {
        self.refresh_outside == Some(self.outside(tracker))
    }

    /// records a refresh that evaluated properties. `fed_back` says whether
    /// it changed the ui's layout targets while nothing outside geometry had
    /// changed.
    pub(crate) fn finish_refresh(&mut self, tracker: &Tracker, fed_back: bool) {
        self.driven = fed_back;
        self.refresh_outside = Some(self.outside(tracker));
    }

    /// node ids and widget sources whose content size layout should measure.
    pub(crate) fn measured(&self) -> (Vec<NodeId>, Vec<u64>) {
        let mut nodes = Vec::new();
        let mut sources = Vec::new();
        for handle in &self.handles {
            let handle = handle.borrow();
            if handle.retired {
                continue;
            }
            match &handle.target {
                Some(target) => nodes.push(target.node()),
                None => sources.push(handle.source),
            }
        }
        (nodes, sources)
    }

    /// frees handles luau no longer references. their revisions return to
    /// the spare list without subscribers, so a recycled signal never
    /// refreshes a property function that read the old handle.
    fn reclaim(&mut self, tracker: &RefCell<Tracker>) {
        let mut tracker = tracker.borrow_mut();
        let spare = &mut self.spare;
        let marks_stale = &mut self.marks_stale;
        self.handles.retain(|handle| {
            if Rc::strong_count(handle) > 1 {
                return true;
            }
            let handle = handle.borrow();
            tracker.clear_subscribers(handle.revision);
            spare.push(handle.revision);
            *marks_stale |= !handle.retired;
            false
        });
    }

    /// follows identities after the accepted tree changed. a handle whose
    /// widget left the tree, or moved to another surface, loses its
    /// measurement now rather than at the next frame.
    pub(crate) fn sync(&mut self, tracker: &RefCell<Tracker>, context: &UiContext) -> Result<()> {
        writable(tracker)?;
        self.reclaim(tracker);
        if self.handles.is_empty() {
            return Ok(());
        }
        let targets = Targets::new(context);
        let mut writes = Vec::new();
        for shared in &self.handles {
            let mut handle = shared.borrow_mut();
            let was_retired = handle.retired;
            let found = lookup(&handle, &targets, context);
            let target = apply(&mut handle, found);
            self.marks_stale |= handle.retired != was_retired;
            let Some(surface) = handle.snapshot.or(handle.measured).map(|s| s.surface) else {
                continue;
            };
            if target
                .as_ref()
                .is_none_or(|target| surface != surface_root(target))
            {
                if handle.snapshot.is_some() {
                    writes.push(handle.revision);
                }
                handle.clear();
            }
        }
        // a chain outlives a closed popup, so reopening it without an outside
        // change continues the same chain. it ends with the surface's root.
        let roots: BTreeSet<_> = context.targets.values().map(surface_root).collect();
        self.chains.retain(|root, _| roots.contains(root));
        self.bump(tracker, writes)
    }

    /// applies one presented frame of surface `root` to every handle on it.
    /// every measurement is read and validated before any handle changes, so
    /// a rejected frame leaves all handles, chains, and revisions as they
    /// were.
    pub(crate) fn publish(
        &mut self,
        tracker: &RefCell<Tracker>,
        context: &UiContext,
        root: NodeId,
        provider: &dyn BoundsProvider,
    ) -> Result<GeometryUpdate> {
        writable(tracker)?;
        if self.handles.is_empty() {
            return Ok(GeometryUpdate::default());
        }
        let targets = Targets::new(context);
        let mut plans = Vec::with_capacity(self.handles.len());
        for shared in &self.handles {
            let handle = shared.borrow();
            let found = lookup(&handle, &targets, context);
            let measured = match &found {
                Lookup::Current(target) if surface_root(target) == root => {
                    Some(measure(provider, target.node(), root)?)
                }
                _ => None,
            };
            plans.push((found, measured));
        }

        let now = Witness {
            outside: self.outside(&tracker.borrow()),
            advances: self.advances,
            commits: self.commits,
        };
        let chain = self.chains.entry(root).or_default();
        // the accepted tree changed since this surface's last frame, so the
        // frame may follow from a refresh that geometry caused.
        let fed = chain.seen.is_some_and(|seen| seen.commits != now.commits);
        let outside = chain
            .seen
            .is_none_or(|seen| seen.outside != now.outside || seen.advances != now.advances);
        // geometry that moved without a new tree, or in a frame the host
        // resized or scrolled, moved for the surface's own reasons.
        let moved = provider.moved()
            || (!fed
                && self
                    .handles
                    .iter()
                    .zip(&plans)
                    .any(|(shared, (_, measured))| {
                        measured.is_some_and(|measured| measured != shared.borrow().measured)
                    }));
        if outside || moved {
            chain.rounds = 0;
        }
        let limited = chain.rounds >= MAX_FEEDBACK_ROUNDS;
        let mut writes = Vec::new();
        let mut update = GeometryUpdate::default();
        for (shared, (found, measured)) in self.handles.iter().zip(plans) {
            let mut handle = shared.borrow_mut();
            let was_retired = handle.retired;
            apply(&mut handle, found);
            self.marks_stale |= handle.retired != was_retired;
            let Some(snapshot) = measured else {
                // a handle that left this surface between the accepted
                // refresh and this frame loses its old measurement.
                if handle
                    .snapshot
                    .or(handle.measured)
                    .is_some_and(|snapshot| snapshot.surface == root)
                {
                    if handle.snapshot.is_some() {
                        writes.push(handle.revision);
                    }
                    handle.clear();
                }
                continue;
            };
            handle.measured = snapshot;
            if snapshot == handle.snapshot {
                continue;
            }
            if limited {
                if !handle.frozen {
                    handle.frozen = true;
                    update.frozen.push(
                        handle
                            .target
                            .as_ref()
                            .map(UiTarget::node)
                            .expect("a measured handle has a target"),
                    );
                }
                continue;
            }
            handle.frozen = false;
            handle.snapshot = snapshot;
            writes.push(handle.revision);
        }
        // an unchanged frame neither extends nor ends a chain: a loop through
        // another surface redraws this one without changing it.
        if !writes.is_empty() {
            chain.rounds += 1;
            update.changed = true;
        }
        chain.seen = Some(now);
        self.bump(tracker, writes)?;
        self.reclaim(tracker);
        Ok(update)
    }

    /// a surface that is no longer presented takes its measurements with it.
    pub(crate) fn forget_surface(
        &mut self,
        tracker: &RefCell<Tracker>,
        root: NodeId,
    ) -> Result<()> {
        writable(tracker)?;
        self.chains.remove(&root);
        let mut writes = Vec::new();
        for shared in &self.handles {
            let mut handle = shared.borrow_mut();
            if handle
                .snapshot
                .or(handle.measured)
                .is_some_and(|snapshot| snapshot.surface == root)
            {
                if handle.snapshot.is_some() {
                    writes.push(handle.revision);
                }
                handle.clear();
            }
        }
        self.bump(tracker, writes)
    }

    /// callers check `writable` before changing any handle, so the batch
    /// cannot fail after a partial update.
    fn bump(&mut self, tracker: &RefCell<Tracker>, revisions: Vec<usize>) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }
        let mut tracker = tracker.borrow_mut();
        let values: Vec<_> = revisions
            .into_iter()
            .map(|id| {
                let next = match tracker.peek(id) {
                    Scalar::Number(value) => value + 1.0,
                    _ => 1.0,
                };
                (id, Scalar::Number(next))
            })
            .collect();
        let before = tracker.changes;
        tracker.write_batch(values)?;
        self.own_changes += tracker.changes - before;
        self.refresh_pending = true;
        Ok(())
    }

    /// finds or makes the handle for a widget source. a retired handle asked
    /// for again starts a fresh identity.
    fn handle(&mut self, lua: &Lua, tracker: &RefCell<Tracker>, source: u64) -> Result<Shared> {
        self.reclaim(tracker);
        if let Some(shared) = self.handles.iter().find(|handle| {
            let handle = handle.borrow();
            handle.source == source && !handle.retired
        }) {
            return Ok(shared.clone());
        }
        if self.handles.len() >= MAX_HANDLES {
            // handles dropped since the last collection still hold their
            // slots until luau finalizes them.
            lua.gc_collect()?;
            self.reclaim(tracker);
        }
        if self.handles.len() >= MAX_HANDLES {
            return Err(Error::runtime(format!(
                "a configuration may hold at most {MAX_HANDLES} geometry handles at once"
            )));
        }
        let revision = match self.spare.pop() {
            Some(revision) => revision,
            None => tracker.borrow_mut().insert(Scalar::Number(0.0))?,
        };
        let shared = Rc::new(RefCell::new(Handle {
            source,
            target: None,
            retired: false,
            revision,
            snapshot: None,
            measured: None,
            frozen: false,
        }));
        self.handles.push(shared.clone());
        self.marks_stale = true;
        Ok(shared)
    }
}

fn writable(tracker: &RefCell<Tracker>) -> Result<()> {
    if tracker.borrow().is_evaluating() {
        return Err(Error::runtime(
            "geometry cannot change while a property function runs",
        ));
    }
    Ok(())
}

fn measure(provider: &dyn BoundsProvider, node: NodeId, root: NodeId) -> Result<Option<Snapshot>> {
    let Some(bounds) = provider.get(node) else {
        return Ok(None);
    };
    bounds.validate(node)?;
    let content = provider.content(node);
    if content.is_some_and(|(width, height)| {
        !width.is_finite() || !height.is_finite() || width < 0.0 || height < 0.0
    }) {
        return Err(Error::runtime(
            "measured content sizes must be finite and nonnegative",
        ));
    }
    Ok(Some(Snapshot {
        surface: root,
        layout: bounds.layout,
        visual: bounds.visual,
        content,
    }))
}

fn lookup(handle: &Handle, targets: &Targets, context: &UiContext) -> Lookup {
    if handle.retired {
        return Lookup::Waiting;
    }
    let current = match &handle.target {
        // keyed re-creation keeps the node id and key while the source
        // changes, and an unrelated widget never matches the whole lineage.
        Some(previous) => targets
            .0
            .get(&previous.node())
            .copied()
            .filter(|current| ui_target_identity_matches(current, previous)),
        None => context.targets.get(&handle.source),
    };
    match (current, &handle.target) {
        (Some(current), _) => Lookup::Current(current.clone()),
        // an identity that was resolved and is now gone never returns.
        (None, Some(_)) => Lookup::Gone,
        (None, None) => Lookup::Waiting,
    }
}

fn apply(handle: &mut Handle, found: Lookup) -> Option<UiTarget> {
    match found {
        Lookup::Current(target) => {
            handle.target = Some(target.clone());
            Some(target)
        }
        Lookup::Gone => {
            handle.retired = true;
            handle.frozen = false;
            None
        }
        Lookup::Waiting => None,
    }
}

fn rect(lua: &Lua, rect: BoundsRect) -> Result<Table> {
    let table = lua.create_table()?;
    table.raw_set("x", rect.x)?;
    table.raw_set("y", rect.y)?;
    table.raw_set("width", rect.width)?;
    table.raw_set("height", rect.height)?;
    table.set_readonly(true);
    Ok(table)
}

pub(crate) fn install(
    lua: &Lua,
    ui: &Table,
    tracker: Rc<RefCell<Tracker>>,
    registry: Rc<RefCell<Geometry>>,
) -> Result<()> {
    ui.set(
        "geometry",
        lua.create_function(move |lua, widget: Value| {
            let source = widget
                .as_table()
                .and_then(|widget| widget.raw_get::<u64>("_id").ok())
                .ok_or_else(|| Error::runtime("ui.geometry target must be a widget"))?;
            // a property function would make or revive a handle on every
            // refresh, and handles change what layout measures.
            if tracker.borrow().is_evaluating() {
                return Err(Error::runtime(
                    "ui.geometry must be called while loading or in an event handler",
                ));
            }
            let shared = registry.borrow_mut().handle(lua, &tracker, source)?;
            let handle = lua.create_table()?;
            for name in ["available", "layout", "visual", "content", "surface"] {
                // each method holds the handle, so a method kept after its
                // table was dropped still reads the same widget.
                let shared = shared.clone();
                let tracker = tracker.clone();
                handle.set(
                    name,
                    lua.create_function(move |lua, ()| -> Result<Value> {
                        let (revision, snapshot) = {
                            let handle = shared.borrow();
                            (handle.revision, handle.snapshot)
                        };
                        // reading subscribes the calling property function.
                        tracker.borrow_mut().read(revision);
                        let Some(snapshot) = snapshot else {
                            return Ok(if name == "available" {
                                Value::Boolean(false)
                            } else {
                                Value::Nil
                            });
                        };
                        Ok(match name {
                            "available" => Value::Boolean(true),
                            "layout" => Value::Table(rect(lua, snapshot.layout)?),
                            "visual" => Value::Table(rect(lua, snapshot.visual)?),
                            "surface" => {
                                Value::String(lua.create_string(snapshot.surface.0.to_string())?)
                            }
                            _ => match snapshot.content {
                                Some((width, height)) => {
                                    let table = lua.create_table()?;
                                    table.raw_set("width", width)?;
                                    table.raw_set("height", height)?;
                                    table.set_readonly(true);
                                    Value::Table(table)
                                }
                                None => Value::Nil,
                            },
                        })
                    })?,
                )?;
            }
            handle.set_readonly(true);
            Ok(handle)
        })?,
    )
}
