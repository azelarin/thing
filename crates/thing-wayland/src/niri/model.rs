//! projection of niri's json event stream into the framework's service values.
//! unknown events are ignored so additions to the protocol remain compatible.

use std::{collections::BTreeMap, io};

use serde::Deserialize;
use serde_json::{Value, json};
use thing_runtime::{
    NiriAction, NiriCast, NiriCastKind, NiriCastTarget, NiriKeyboardLayouts, NiriLayoutTarget,
    NiriState, NiriWindow, Workspace,
};

#[derive(Default)]
pub(super) struct Model {
    workspaces: BTreeMap<u64, Workspace>,
    windows: BTreeMap<u64, NiriWindow>,
    workspaces_initialized: bool,
    windows_initialized: bool,
    overview_open: Option<bool>,
    keyboard_layouts: Option<NiriKeyboardLayouts>,
    casts: Option<BTreeMap<u64, NiriCast>>,
}

impl Model {
    pub fn is_initialized(&self) -> bool {
        self.workspaces_initialized && self.windows_initialized
    }

    pub fn snapshot(&self) -> NiriState {
        let mut workspaces: Vec<_> = self.workspaces.values().cloned().collect();
        workspaces.sort_by(|a, b| (&a.output, a.index).cmp(&(&b.output, b.index)));
        NiriState {
            workspaces,
            windows: self.windows.values().cloned().collect(),
            overview_open: self.overview_open,
            keyboard_layouts: self.keyboard_layouts.clone(),
            casts: self
                .casts
                .as_ref()
                .map(|casts| casts.values().cloned().collect()),
        }
    }

    pub fn apply(&mut self, line: &[u8]) -> io::Result<bool> {
        let value: Value = serde_json::from_slice(line)?;
        let object = value
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| io::Error::other("invalid niri event envelope"))?;
        let name = object.keys().next().expect("one event key");
        if !matches!(
            name.as_str(),
            "WorkspacesChanged"
                | "WorkspaceActivated"
                | "WorkspaceUrgencyChanged"
                | "WindowsChanged"
                | "WindowOpenedOrChanged"
                | "WindowFocusChanged"
                | "WindowClosed"
                | "WindowLayoutsChanged"
                | "WindowUrgencyChanged"
                | "OverviewOpenedOrClosed"
                | "KeyboardLayoutsChanged"
                | "KeyboardLayoutSwitched"
                | "CastsChanged"
                | "CastStartedOrChanged"
                | "CastStopped"
        ) {
            return Ok(false);
        }
        match serde_json::from_value::<Event>(value)? {
            Event::WorkspacesChanged { workspaces } => {
                check_count(workspaces.len())?;
                self.workspaces = workspaces.into_iter().map(|w| (w.id, w.into())).collect();
                self.workspaces_initialized = true;
            }
            Event::WorkspaceActivated { id, focused } => {
                let output = self
                    .workspaces
                    .get(&id)
                    .ok_or_else(|| io::Error::other("niri activated an unknown workspace"))?
                    .output
                    .clone();
                for w in self.workspaces.values_mut() {
                    if w.output == output {
                        w.active = w.id == id;
                    }
                    if focused {
                        w.focused = w.id == id;
                    }
                }
            }
            Event::WorkspaceUrgencyChanged { id, urgent } => {
                if let Some(w) = self.workspaces.get_mut(&id) {
                    w.urgent = urgent;
                }
            }
            Event::OverviewOpenedOrClosed { is_open } => self.overview_open = Some(is_open),
            Event::KeyboardLayoutsChanged { keyboard_layouts } => {
                let count = keyboard_layouts.names.len();
                let index = usize::from(keyboard_layouts.current_idx);
                if count > 256
                    || (count == 0 && index != 0)
                    || (count != 0 && index >= count)
                    || keyboard_layouts
                        .names
                        .iter()
                        .any(|name| name.len() > 65536 || name.contains('\0'))
                {
                    return Err(io::Error::other("invalid niri keyboard layouts"));
                }
                self.keyboard_layouts = Some(NiriKeyboardLayouts {
                    names: keyboard_layouts.names,
                    current_index: index,
                });
            }
            Event::KeyboardLayoutSwitched { idx } => {
                let layouts = self
                    .keyboard_layouts
                    .as_mut()
                    .ok_or_else(|| io::Error::other("niri switched an unknown keyboard layout"))?;
                if usize::from(idx) >= layouts.names.len() {
                    return Err(io::Error::other("niri switched an unknown keyboard layout"));
                }
                layouts.current_index = usize::from(idx);
            }
            Event::CastsChanged { casts } => {
                check_count(casts.len())?;
                let mut replacement = BTreeMap::new();
                for cast in casts {
                    let cast = NiriCast::try_from(cast)?;
                    if replacement.insert(cast.stream_id, cast).is_some() {
                        return Err(io::Error::other("duplicate niri cast stream id"));
                    }
                }
                self.casts = Some(replacement);
            }
            Event::CastStartedOrChanged { cast } => {
                let cast = NiriCast::try_from(cast)?;
                let Some(casts) = self.casts.as_mut() else {
                    // a delta cannot establish a complete list without its initial snapshot.
                    return Ok(false);
                };
                check_count(casts.len() + usize::from(!casts.contains_key(&cast.stream_id)))?;
                casts.insert(cast.stream_id, cast);
            }
            Event::CastStopped { stream_id } => {
                let Some(casts) = self.casts.as_mut() else {
                    return Ok(false);
                };
                casts.remove(&stream_id);
            }
            Event::WindowsChanged { windows } => {
                check_count(windows.len())?;
                self.windows = windows.into_iter().map(|w| (w.id, w.into())).collect();
                self.windows_initialized = true;
            }
            Event::WindowOpenedOrChanged { window } => {
                if window.is_focused {
                    for w in self.windows.values_mut() {
                        w.focused = false;
                    }
                }
                self.windows.insert(window.id, window.into());
                check_count(self.windows.len())?;
            }
            Event::WindowFocusChanged { id } => {
                for w in self.windows.values_mut() {
                    w.focused = Some(w.id) == id;
                }
            }
            Event::WindowClosed { id } => {
                self.windows.remove(&id);
            }
            Event::WindowUrgencyChanged { id, urgent } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.urgent = urgent;
                }
            }
            Event::WindowLayoutsChanged { changes } => {
                for (id, layout) in changes {
                    if let Some(w) = self.windows.get_mut(&id) {
                        layout.apply(w);
                    }
                }
            }
        }
        Ok(true)
    }
}

fn check_count(count: usize) -> io::Result<()> {
    if count > 10000 {
        Err(io::Error::other("niri snapshot exceeds 10,000 items"))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
enum Event {
    WorkspacesChanged {
        workspaces: Vec<WireWorkspace>,
    },
    WorkspaceActivated {
        id: u64,
        focused: bool,
    },
    WorkspaceUrgencyChanged {
        id: u64,
        urgent: bool,
    },
    WindowsChanged {
        windows: Vec<WireWindow>,
    },
    WindowOpenedOrChanged {
        window: WireWindow,
    },
    WindowFocusChanged {
        id: Option<u64>,
    },
    WindowClosed {
        id: u64,
    },
    WindowLayoutsChanged {
        changes: Vec<(u64, WireLayout)>,
    },
    WindowUrgencyChanged {
        id: u64,
        urgent: bool,
    },
    OverviewOpenedOrClosed {
        is_open: bool,
    },
    KeyboardLayoutsChanged {
        keyboard_layouts: WireKeyboardLayouts,
    },
    KeyboardLayoutSwitched {
        idx: u8,
    },
    CastsChanged {
        casts: Vec<WireCast>,
    },
    CastStartedOrChanged {
        cast: WireCast,
    },
    CastStopped {
        stream_id: u64,
    },
}

#[derive(Deserialize)]
struct WireCast {
    stream_id: u64,
    session_id: u64,
    kind: String,
    target: Value,
    is_dynamic_target: bool,
    is_active: bool,
    pid: Option<i32>,
    pw_node_id: Option<u32>,
}

#[derive(Deserialize)]
enum WireCastTarget {
    Nothing {},
    Output { name: String },
    Window { id: u64 },
}

impl TryFrom<WireCast> for NiriCast {
    type Error = io::Error;

    fn try_from(cast: WireCast) -> io::Result<Self> {
        let checked_text = |text: String| {
            if text.len() > 65536 || text.contains('\0') {
                Err(io::Error::other("invalid niri cast text"))
            } else {
                Ok(text)
            }
        };
        let kind = match cast.kind.as_str() {
            "PipeWire" => NiriCastKind::PipeWire,
            "WlrScreencopy" => NiriCastKind::WlrScreencopy,
            _ => NiriCastKind::Unknown(checked_text(cast.kind)?),
        };
        let tag = cast
            .target
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.keys().next())
            .ok_or_else(|| io::Error::other("invalid niri cast target"))?;
        let target = if matches!(tag.as_str(), "Nothing" | "Output" | "Window") {
            match serde_json::from_value::<WireCastTarget>(cast.target)? {
                WireCastTarget::Nothing {} => NiriCastTarget::Nothing,
                WireCastTarget::Output { name } => NiriCastTarget::Output(checked_text(name)?),
                WireCastTarget::Window { id } => NiriCastTarget::Window(id),
            }
        } else {
            // preserve the stream when a newer compositor adds a target type.
            NiriCastTarget::Unknown(checked_text(tag.clone())?)
        };
        Ok(Self {
            stream_id: cast.stream_id,
            session_id: cast.session_id,
            kind,
            target,
            dynamic_target: cast.is_dynamic_target,
            active: cast.is_active,
            pid: cast.pid,
            pipewire_node_id: cast.pw_node_id,
        })
    }
}

#[derive(Deserialize)]
struct WireKeyboardLayouts {
    names: Vec<String>,
    current_idx: u8,
}

#[derive(Deserialize)]
struct WireWorkspace {
    id: u64,
    idx: u8,
    name: Option<String>,
    output: Option<String>,
    is_active: bool,
    is_focused: bool,
    #[serde(default)]
    is_urgent: bool,
}

impl From<WireWorkspace> for Workspace {
    fn from(w: WireWorkspace) -> Self {
        Self {
            id: w.id,
            index: w.idx,
            name: w.name,
            output: w.output,
            active: w.is_active,
            focused: w.is_focused,
            urgent: w.is_urgent,
        }
    }
}

#[derive(Deserialize)]
struct WireWindow {
    id: u64,
    workspace_id: Option<u64>,
    title: Option<String>,
    app_id: Option<String>,
    is_focused: bool,
    is_floating: bool,
    #[serde(default)]
    is_urgent: bool,
    layout: WireLayout,
}

#[derive(Deserialize)]
struct WireLayout {
    pos_in_scrolling_layout: Option<(usize, usize)>,
    tile_size: (f64, f64),
}

impl WireLayout {
    fn apply(self, window: &mut NiriWindow) {
        window.column = self.pos_in_scrolling_layout.map(|p| p.0);
        window.row = self.pos_in_scrolling_layout.map(|p| p.1);
        window.width = self.tile_size.0;
        window.height = self.tile_size.1;
    }
}

impl From<WireWindow> for NiriWindow {
    fn from(w: WireWindow) -> Self {
        let mut window = Self {
            id: w.id,
            workspace_id: w.workspace_id,
            title: w.title,
            app_id: w.app_id,
            focused: w.is_focused,
            floating: w.is_floating,
            urgent: w.is_urgent,
            column: None,
            row: None,
            width: 0.0,
            height: 0.0,
        };
        w.layout.apply(&mut window);
        window
    }
}

pub(super) fn request(action: NiriAction) -> Value {
    json!({"Action": match action {
        NiriAction::FocusWorkspace(id) => json!({"FocusWorkspace": {"reference": {"Id": id}}}),
        NiriAction::FocusWindow(id) => json!({"FocusWindow": {"id": id}}),
        NiriAction::CloseWindow(id) => json!({"CloseWindow": {"id": id}}),
        NiriAction::SetOverview(true) => json!({"OpenOverview": {}}),
        NiriAction::SetOverview(false) => json!({"CloseOverview": {}}),
        NiriAction::ToggleOverview => json!({"ToggleOverview": {}}),
        NiriAction::MoveWindowToWorkspace { window_id, workspace_id, focus } => {
            json!({"MoveWindowToWorkspace": {"window_id": window_id, "reference": {"Id": workspace_id}, "focus": focus}})
        },
        NiriAction::MoveWindowToOutput { window_id, output } => {
            json!({"MoveWindowToMonitor": {"id": window_id, "output": output}})
        },
        NiriAction::MoveWorkspaceToOutput { workspace_id, output } => {
            json!({"MoveWorkspaceToMonitor": {"reference": {"Id": workspace_id}, "output": output}})
        },
        NiriAction::SwitchLayout(layout) => json!({"SwitchLayout": {"layout": match layout {
            NiriLayoutTarget::Next => json!("Next"),
            NiriLayoutTarget::Previous => json!("Prev"),
            NiriLayoutTarget::Index(index) => json!({"Index": index}),
        }}}),
    }})
}

pub(super) fn reply(line: &[u8]) -> io::Result<()> {
    match serde_json::from_slice::<Result<String, String>>(line)? {
        Ok(value) if value == "Handled" => Ok(()),
        Ok(_) => Err(io::Error::other("unexpected niri response")),
        Err(error) => Err(io::Error::other(error)),
    }
}
