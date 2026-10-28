//! embedded luau configurations and dependency-tracked widget properties.
//!
//! a [`Runtime`] owns a loaded configuration and its state. it exposes a validated
//! [`thing_core::Node`] tree to the renderer.

mod modules;
mod niri;
mod properties;
mod reconcile;
mod services;
mod state;
pub use niri::{NiriAction, NiriState, NiriWindow, Workspace};
pub use services::{Action, AudioDevice, AudioSnapshot, AudioStream, ServiceSnapshot};

use mlua::{Error, Function, Lua, Result, Table, Value, VmState};
pub use state::Scalar;
use state::{Signal, Tracker};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use thing_core::{Kind, Node, NodeId, Scale};

/// the wayland output for one runtime, measured in logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputContext {
    pub name: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale: Scale,
}

impl OutputContext {
    fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self
                .name
                .as_ref()
                .is_some_and(|name| name.len() > 4096 || name.contains('\0'))
        {
            return Err(Error::runtime("invalid output context"));
        }
        Ok(())
    }
}

/// limits apply to each load, event callback, and batch of reactive updates.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub execution_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            execution_time: Duration::from_millis(250),
        }
    }
}

/// input events are separate from pure property functions.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Click,
    MiddleClick,
    SecondaryClick,
    /// scroll steps; positive values move right or down.
    Scroll {
        x: f64,
        y: f64,
    },
    Change(f64),
    Hover(bool),
    Close,
}

struct Binding {
    node: NodeId,
    property: String,
    function: Function,
}

type Handlers = BTreeMap<(NodeId, &'static str), Function>;

struct Children {
    nodes: Vec<Node>,
    bindings: BTreeMap<usize, Binding>,
    handlers: Handlers,
    sources: BTreeMap<NodeId, u64>,
}

/// a loaded configuration, its state, and the current widget tree.
pub struct Runtime {
    lua: Lua,
    root: Node,
    tracker: Rc<RefCell<Tracker>>,
    bindings: BTreeMap<usize, Binding>,
    next_binding: usize,
    next_node: u64,
    sources: BTreeMap<NodeId, u64>,
    deferred: BTreeSet<usize>,
    handlers: Handlers,
    ended_hovers: BTreeSet<NodeId>,
    deadline: Rc<Cell<Instant>>,
    limits: Limits,
    modules: modules::Modules,
    services: services::Services,
    output: Option<OutputContext>,
}

impl Runtime {
    /// load an entry file and its relative imports without connecting to wayland.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_limits(path, Limits::default())
    }

    pub fn load_with_limits(path: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        Self::load_with_services(path.as_ref(), limits, ServiceSnapshot::default(), None)
    }

    /// load an entry file for one output with current desktop service values.
    pub fn load_for_output(
        path: impl AsRef<Path>,
        output: OutputContext,
        services: &ServiceSnapshot,
    ) -> Result<Self> {
        output.validate()?;
        Self::load_with_services(
            path.as_ref(),
            Limits::default(),
            services.clone(),
            Some(output),
        )
    }

    fn load_with_services(
        path: &Path,
        limits: Limits,
        snapshot: ServiceSnapshot,
        output: Option<OutputContext>,
    ) -> Result<Self> {
        if limits.memory_bytes == 0 || limits.execution_time.is_zero() {
            return Err(Error::runtime("runtime limits must be positive"));
        }
        let lua = Lua::new();
        lua.set_memory_limit(limits.memory_bytes)?;
        let deadline = Rc::new(Cell::new(Instant::now() + limits.execution_time));
        let interrupt_deadline = deadline.clone();
        lua.set_interrupt(move |_| {
            if Instant::now() >= interrupt_deadline.get() {
                Err(Error::runtime("script exceeded its execution time limit"))
            } else {
                Ok(VmState::Continue)
            }
        });
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = services::Services::install(&lua, tracker.clone())?;
        for (name, value) in snapshot.values {
            services.set(&name, value)?;
        }
        services.set_devices(snapshot.devices)?;
        services.set_sources(snapshot.sources)?;
        services.set_streams(snapshot.streams)?;
        services.set_niri(snapshot.niri)?;
        if let Some(output) = &output {
            services.set_output(output)?;
        }
        install_ui(&lua, tracker.clone())?;
        lua.sandbox(true)?;
        let modules = modules::Modules::default();
        let exported = modules.load(&lua, path)?;
        let mut runtime = Self {
            lua,
            root: Node::new(NodeId(0), Kind::Panel),
            tracker,
            bindings: BTreeMap::new(),
            next_binding: 0,
            next_node: 1,
            sources: BTreeMap::new(),
            deferred: BTreeSet::new(),
            handlers: BTreeMap::new(),
            ended_hovers: BTreeSet::new(),
            deadline,
            limits,
            modules,
            services,
            output,
        };
        runtime.root = runtime.parse_node(exported, 0, &mut BTreeSet::new())?;
        if runtime.root.kind != Kind::Panel {
            return Err(Error::runtime(
                "the entry file must return ui.panel { ... }",
            ));
        }
        runtime.root.validate().map_err(Error::external)?;
        Ok(runtime)
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    /// files loaded by the configuration, including its entry file.
    pub fn files(&self) -> Vec<PathBuf> {
        self.modules.files()
    }

    pub fn memory_used(&self) -> usize {
        self.lua.used_memory()
    }

    /// sets a service value. call [`Self::refresh`] after updating all fields.
    pub fn set_service(&mut self, name: &str, value: Scalar) -> Result<()> {
        self.services.set(name, value)
    }

    /// replaces the audio output list and marks its subscribers for update.
    pub fn set_audio_devices(&mut self, devices: Vec<AudioDevice>) -> Result<()> {
        self.services.set_devices(devices)
    }

    /// replaces the audio input list and marks its subscribers for update.
    pub fn set_audio_sources(&mut self, sources: Vec<AudioDevice>) -> Result<()> {
        self.services.set_sources(sources)
    }

    /// replaces output and input audio state in one update. call [`Self::refresh`] afterward.
    pub fn set_audio(&mut self, audio: AudioSnapshot) -> Result<()> {
        self.services.set_audio(audio)
    }

    /// replaces workspace and window lists. call [`Self::refresh`] to update widgets.
    pub fn set_niri(&mut self, state: NiriState) -> Result<()> {
        self.services.set_niri(state)
    }

    /// replace all output properties together. call [`Self::refresh`] afterward.
    pub fn set_output(&mut self, output: OutputContext) -> Result<()> {
        self.services.set_output(&output)?;
        self.output = Some(output);
        Ok(())
    }

    /// takes the service actions queued by event handlers.
    pub fn take_actions(&mut self) -> Vec<Action> {
        self.services.take_actions()
    }

    /// whether an available widget has an explicit scroll handler.
    pub fn has_scroll_handler(&self, id: NodeId) -> bool {
        is_available(&self.root, id) && self.handlers.contains_key(&(id, "on_scroll"))
    }

    /// run an input handler. disabled or hidden widgets ignore input.
    pub fn dispatch(&mut self, id: NodeId, event: Event) -> Result<bool> {
        if matches!(event, Event::Hover(false)) && self.ended_hovers.remove(&id) {
            return Ok(false);
        }
        if matches!(event, Event::Hover(true)) && self.ended_hovers.contains(&id) {
            return Ok(false);
        }
        if !matches!(event, Event::Close | Event::Hover(false)) && !is_available(&self.root, id) {
            return Ok(false);
        }
        let name = match event {
            Event::Click => "on_click",
            Event::MiddleClick => "on_middle_click",
            Event::SecondaryClick => "on_secondary_click",
            Event::Scroll { x, y } if x.is_finite() && y.is_finite() => "on_scroll",
            Event::Scroll { .. } => return Err(Error::runtime("scroll deltas must be finite")),
            Event::Close => "on_close",
            Event::Hover(_) => "on_hover",
            Event::Change(value) if value.is_finite() => "on_change",
            Event::Change(_) => return Err(Error::runtime("input value must be finite")),
        };
        let Some(handler) = self.handlers.get(&(id, name)).cloned() else {
            return Ok(false);
        };
        self.begin_call();
        let checkpoint = self.services.action_count();
        self.services.allow_actions(true);
        let result = match event {
            Event::Click | Event::MiddleClick | Event::SecondaryClick | Event::Close => {
                handler.call::<()>(())
            }
            Event::Scroll { x, y } => handler.call::<()>((x, y)),
            Event::Change(value) => handler.call::<()>(value),
            Event::Hover(inside) => handler.call::<()>(inside),
        };
        self.services.allow_actions(false);
        let result = result.and_then(|()| self.refresh());
        let result = match result {
            Ok(changed)
                if matches!(event, Event::Hover(true))
                    && (self.root.find(id).is_none()
                        || !self.handlers.contains_key(&(id, "on_hover"))) =>
            {
                self.ended_hovers.insert(id);
                self.begin_call();
                self.services.allow_actions(true);
                let cleanup = handler.call::<()>(false);
                self.services.allow_actions(false);
                cleanup
                    .and_then(|()| self.refresh())
                    .map(|cleanup_changed| changed || cleanup_changed)
            }
            result => result,
        };
        if result.is_err() {
            self.services.truncate_actions(checkpoint);
        }
        result
    }

    /// reevaluate only dirty properties and publish them together after validation.
    /// an error preserves the previous snapshot. a later state change can retry.
    pub fn refresh(&mut self) -> Result<bool> {
        let mut dirty = std::mem::take(&mut self.tracker.borrow_mut().dirty);
        if dirty.is_empty() {
            return Ok(false);
        }
        dirty.append(&mut self.deferred);
        // retry a failed batch when another dependency changes. retaining the
        // whole batch prevents unrelated, valid changes from being lost.
        self.deferred = dirty.clone();
        self.begin_call();
        let mut candidate = self.root.clone();
        let checkpoint = self.next_binding;
        let mut retired = BTreeSet::new();
        let mut removed = BTreeSet::new();
        let mut handlers = Handlers::new();
        let mut sources = self.sources.clone();
        let result = (|| {
            for index in dirty {
                if retired.contains(&index) {
                    continue;
                }
                let value = self.evaluate_binding(index)?;
                let binding = &self.bindings[&index];
                let id = binding.node;
                let property = binding.property.clone();
                if property == "children" {
                    let depth = reconcile::depth(&candidate, id)
                        .ok_or_else(|| Error::runtime("list refers to a missing widget"))?;
                    let node = candidate.find_mut(id).expect("depth located the widget");
                    let mut old_ids = BTreeSet::new();
                    for child in &node.children {
                        reconcile::ids(child, &mut old_ids);
                    }
                    sources.retain(|id, _| !old_ids.contains(id));
                    let mut seen = sources.values().copied().collect();
                    let mut children = self.detached_children(value, depth + 1, &mut seen)?;
                    let mapping = reconcile::preserve(&node.children, &mut children.nodes);
                    for binding in children.bindings.values_mut() {
                        binding.node = mapping.get(&binding.node).copied().unwrap_or(binding.node);
                    }
                    handlers.extend(children.handlers.into_iter().map(
                        |((id, event), function)| {
                            ((mapping.get(&id).copied().unwrap_or(id), event), function)
                        },
                    ));
                    sources.extend(
                        children
                            .sources
                            .into_iter()
                            .map(|(id, source)| (mapping.get(&id).copied().unwrap_or(id), source)),
                    );
                    retired.extend(self.bindings.iter().filter_map(|(index, binding)| {
                        old_ids.contains(&binding.node).then_some(*index)
                    }));
                    removed.extend(old_ids);
                    self.bindings.extend(children.bindings);
                    node.children = children.nodes;
                } else {
                    let node = candidate.find_mut(id).ok_or_else(|| {
                        Error::runtime("reactive binding refers to a missing widget")
                    })?;
                    properties::apply(node, &property, value)?;
                }
            }
            candidate.validate().map_err(Error::external)
        })();
        if let Err(error) = result {
            // only bindings made for the rejected candidate are discarded.
            // existing subscriptions and handlers continue serving the old ui.
            for index in self.bindings.split_off(&checkpoint).keys() {
                self.tracker.borrow_mut().forget(*index);
            }
            return Err(error);
        }
        for index in retired {
            self.bindings.remove(&index);
            self.tracker.borrow_mut().forget(index);
        }
        self.handlers.retain(|(id, _), _| !removed.contains(id));
        self.handlers.extend(handlers);
        self.sources = sources;
        let changed = candidate != self.root;
        self.root = candidate;
        self.deferred.clear();
        Ok(changed)
    }

    /// load a replacement without changing the current runtime.
    pub fn load_replacement(&self, path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_services(
            path.as_ref(),
            self.limits,
            self.services.snapshot(),
            self.output.clone(),
        )
    }

    /// reload the configuration, preserving it if loading or validation fails.
    pub fn reload(&mut self, path: impl AsRef<Path>) -> Result<()> {
        *self = self.load_replacement(path)?;
        Ok(())
    }

    fn begin_call(&self) {
        self.deadline
            .set(Instant::now() + self.limits.execution_time);
    }

    fn evaluate_binding(&self, index: usize) -> Result<Value> {
        self.tracker.borrow_mut().begin(index);
        let result = self.bindings[&index].function.call::<Value>(());
        self.tracker.borrow_mut().finish(index);
        result
    }

    fn parse_node(&mut self, value: Value, depth: usize, seen: &mut BTreeSet<u64>) -> Result<Node> {
        if depth > 64 || seen.len() >= 10_000 {
            return Err(Error::runtime(
                "widget tree exceeds 64 levels or 10,000 nodes",
            ));
        }
        let Value::Table(table) = value else {
            return Err(Error::runtime("expected a widget returned by ui.*"));
        };
        let source: u64 = table.raw_get("_id")?;
        if !seen.insert(source) {
            return Err(Error::runtime(
                "a widget is reused or the tree contains a cycle",
            ));
        }
        let kind = kind(&table.raw_get::<String>("_kind")?)?;
        let id = NodeId(self.next_node);
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| Error::runtime("too many widget identities"))?;
        self.sources.insert(id, source);
        let mut node = Node::new(id, kind);
        for pair in table.clone().pairs::<String, Value>() {
            let (key, value) = pair?;
            match key.as_str() {
                "_id" | "_kind" | "children" => {}
                "edge" | "margins" | "exclusive_zone" | "input_region" if depth > 0 => {
                    return Err(Error::runtime(format!(
                        "{key} is only supported on the root panel"
                    )));
                }
                "key" => node.key = Some(properties::string(value, "key")?),
                "on_click" | "on_middle_click" | "on_secondary_click" | "on_scroll"
                | "on_change" | "on_hover" | "on_close" => {
                    if (matches!(
                        key.as_str(),
                        "on_click" | "on_middle_click" | "on_secondary_click" | "on_scroll"
                    ) && kind != Kind::Button)
                        || (key == "on_change" && kind != Kind::Slider)
                        || (key == "on_hover" && kind != Kind::Popup && !kind.is_interactive())
                        || (key == "on_close" && kind != Kind::Popup)
                    {
                        return Err(Error::runtime(format!(
                            "{key} is not supported on {kind:?}"
                        )));
                    }
                    let Value::Function(function) = value else {
                        return Err(Error::runtime(format!("{key} must be a function")));
                    };
                    let name = match key.as_str() {
                        "on_click" => "on_click",
                        "on_middle_click" => "on_middle_click",
                        "on_secondary_click" => "on_secondary_click",
                        "on_scroll" => "on_scroll",
                        "on_change" => "on_change",
                        "on_hover" => "on_hover",
                        _ => "on_close",
                    };
                    self.handlers.insert((node.id, name), function);
                }
                "style" => {
                    let Value::Table(style) = value else {
                        return Err(Error::runtime("style must be a table"));
                    };
                    for pair in style.pairs::<String, Value>() {
                        let (property, value) = pair?;
                        self.property(&mut node, format!("style.{property}"), value)?;
                    }
                }
                _ => self.property(&mut node, key, value)?,
            }
        }
        let children = table.raw_get::<Value>("children")?;
        if !matches!(children, Value::Nil) {
            let children = if let Value::Function(function) = children {
                let index = self.bind(node.id, "children".into(), function)?;
                self.evaluate_binding(index)?
            } else {
                children
            };
            node.children = self.parse_children(children, depth + 1, seen)?;
        }
        Ok(node)
    }

    fn property(&mut self, node: &mut Node, property: String, value: Value) -> Result<()> {
        let value = if let Value::Function(function) = value {
            let index = self.bind(node.id, property.clone(), function)?;
            self.evaluate_binding(index)?
        } else {
            value
        };
        properties::apply(node, &property, value)
    }

    fn bind(&mut self, node: NodeId, property: String, function: Function) -> Result<usize> {
        let index = self.next_binding;
        self.next_binding = index
            .checked_add(1)
            .ok_or_else(|| Error::runtime("too many bindings"))?;
        self.bindings.insert(
            index,
            Binding {
                node,
                property,
                function,
            },
        );
        Ok(index)
    }

    fn parse_children(
        &mut self,
        value: Value,
        depth: usize,
        seen: &mut BTreeSet<u64>,
    ) -> Result<Vec<Node>> {
        let Value::Table(children) = value else {
            return Err(Error::runtime("children must be a dense array of widgets"));
        };
        let count = children.clone().pairs::<Value, Value>().count();
        if count != children.raw_len() {
            return Err(Error::runtime("children must be a dense array of widgets"));
        }
        (1..=count)
            .map(|index| self.parse_node(children.raw_get(index)?, depth, seen))
            .collect()
    }

    fn detached_children(
        &mut self,
        value: Value,
        depth: usize,
        seen: &mut BTreeSet<u64>,
    ) -> Result<Children> {
        // stage callback registrations so invalid rows cannot replace a working handler.
        let bindings = std::mem::take(&mut self.bindings);
        let handlers = std::mem::take(&mut self.handlers);
        let sources = std::mem::take(&mut self.sources);
        let result = self.parse_children(value, depth, seen);
        let bindings = std::mem::replace(&mut self.bindings, bindings);
        let handlers = std::mem::replace(&mut self.handlers, handlers);
        let sources = std::mem::replace(&mut self.sources, sources);
        match result {
            Ok(nodes) => Ok(Children {
                nodes,
                bindings,
                handlers,
                sources,
            }),
            Err(error) => {
                for index in bindings.keys() {
                    self.tracker.borrow_mut().forget(*index);
                }
                Err(error)
            }
        }
    }
}

fn is_available(node: &Node, id: NodeId) -> bool {
    node.visible
        && node.enabled
        && (node.id == id || node.children.iter().any(|n| is_available(n, id)))
}

fn kind(name: &str) -> Result<Kind> {
    match name {
        "panel" => Ok(Kind::Panel),
        "popup" => Ok(Kind::Popup),
        "row" => Ok(Kind::Row),
        "centerbox" => Ok(Kind::CenterBox),
        "column" => Ok(Kind::Column),
        "stack" => Ok(Kind::Stack),
        "scroll" => Ok(Kind::Scroll),
        "text" => Ok(Kind::Text),
        "image" => Ok(Kind::Image),
        "button" => Ok(Kind::Button),
        "slider" => Ok(Kind::Slider),
        _ => Err(Error::runtime(format!("unknown widget {name:?}"))),
    }
}

fn install_ui(lua: &Lua, tracker: Rc<RefCell<Tracker>>) -> Result<()> {
    let ui = lua.create_table()?;
    let next_id = Rc::new(Cell::new(1_u64));
    for name in [
        "panel",
        "popup",
        "row",
        "centerbox",
        "column",
        "stack",
        "scroll",
        "text",
        "image",
        "button",
        "slider",
    ] {
        let next_id = next_id.clone();
        ui.set(
            name,
            lua.create_function(move |lua, props: Table| {
                let result = lua.create_table()?;
                for pair in props.pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    result.raw_set(key, value)?;
                }
                let id = next_id.get();
                next_id.set(
                    id.checked_add(1)
                        .ok_or_else(|| Error::runtime("too many widgets"))?,
                );
                result.raw_set("_id", id)?;
                result.raw_set("_kind", name)?;
                Ok(result)
            })?,
        )?;
    }
    ui.set(
        "state",
        lua.create_function(move |_, value: Value| {
            let value = Scalar::from_lua(value)?;
            let id = tracker.borrow_mut().insert(value)?;
            Ok(Signal {
                id,
                tracker: tracker.clone(),
                readonly: false,
            })
        })?,
    )?;
    ui.set_readonly(true);
    lua.globals().set("ui", ui)
}
