# configuration

a configuration is a utf-8 `.luau` file that returns one `ui.panel`. type
annotations are accepted, but `thing check` does not perform static type checking.

## your first panel

create a file called `panel.luau` with the following contents:

```luau
local count = ui.state(0)

return ui.panel {
    style = { height = 36, padding = 6, background = '#191b22' },
    children = {
        ui.button {
            style = { padding_x = 8, radius = 4, background = '#343c50' },
            on_click = function() count:set(count:get() + 1) end,
            children = {
                ui.text {
                    text = function() return `clicked {count:get()} times` end,
                    style = { foreground = '#e5e9f0' },
                },
            },
        },
    },
}
```

run it from a terminal in your wayland session:

```sh
thing run ./panel.luau
```

you should see a bar with a counter button. clicking it changes `count`, and the
text updates because its function reads that state. press ctrl+c in the terminal
to close the panel.

try changing the background color while it's running. thing reloads the file
when you save, which also resets the counter. if an edit has an error, thing
reports it in the terminal and keeps the previous configuration running.

use `thing check ./panel.luau` to check a file without opening a panel. the
sections below explain the rest of the api.

## outputs

thing runs a separate configuration for each connected output. each has its own
local state, so changing a counter on one monitor doesn't change the counters on
other monitors. desktop services such as audio still describe the same system.

use `services.output` to read the output that owns your panel:

| value | meaning |
| --- | --- |
| `available` | whether the configuration has a real output |
| `name` | the compositor's output name, or an empty string if unavailable |
| `width`, `height` | the output size in logical pixels, not the panel size |
| `scale` | the rendering scale, such as `1` or `1.25` |

these values are read-only signals. read them inside property functions to
follow changes:

```luau
ui.text {
    text = function()
        local output = services.output
        return `{output.name:get()}: {output.width:get()} × {output.height:get()}`
    end,
}
```

you can also read them during setup to choose a different layout for a particular
output. those reads capture the current value; they don't subscribe to changes.
the initial scale comes from the compositor's integer output scale. it can
update after the compositor supplies its preferred fractional scale, so use a
property function when your layout depends on it.

`thing check` runs without a display. its output is unavailable, with an empty
name, zero width and height, and a scale of `1`. output-specific branches still
need to be tried on the desktop.

when you save a configuration, thing prepares a replacement for every output.
if one fails, all outputs keep their current configuration. a successful reload
resets each output's local state and closes its popups.

## widgets and children

the current model has `panel`, `popup`, `row`, `centerbox`, `column`, `stack`,
`scroll`, `text`, `image`, `button`, and `slider` constructors. pass properties in
a table and children in a dense array:

```luau
return ui.panel {
    children = {
        ui.row {
            style = { gap = 8 },
            children = {
                ui.text { text = 'hello' },
                ui.text { text = 'world' },
            },
        },
    },
}
```

a widget may appear in one position only. to reuse a design, write a function that
creates a fresh widget. unknown properties are errors. trees are limited to 64 levels and 10,000 nodes.

every widget accepts `visible`, `enabled`, and an optional string `key`. a hidden
or disabled ancestor also blocks input to its descendants. keys must be unique
among siblings. function-valued children preserve identity by sibling key and
widget kind; unkeyed children match by position and kind.

| widget | additional properties |
| --- | --- |
| `text` | `text`: utf-8 string |
| `image` | one of `source`, `icon`, `app_id`; optional `icon_theme` |
| `button` | `children`, `on_click`, `on_middle_click`, `on_secondary_click`, `on_scroll`, `on_hover` |
| `slider` | `min`, `max`, `value`, `step`, `on_change`, `on_hover` |
| `popup` | `visible`, `open_on_hover`, `open_on_click`, `open_delay`, `close_delay`, `on_hover`, `on_close` |
| containers | `children`: array of widgets, or a function returning one |

`centerbox` takes exactly three non-popup children: start, center, and end. give
it an explicit height and width (or `grow = 1` inside a row). the middle child
stays centered even when the side groups have different widths. hidden children
keep their slot. groups can overlap if they exceed the available space; choose
which content to hide in your configuration on narrow outputs.

`scroll` lays out its children vertically and clips them to its bounds. give it
an explicit height, or use `grow = 1` inside a column whose height is already
bounded. otherwise it grows to fit its children and has nowhere to scroll. one
wheel step moves 40 logical pixels; high-resolution input keeps fractional
movement. nested viewports keep the event until they reach the limit in that
direction, then the next event moves the outer viewport. an `on_scroll` handler
on the button under the pointer takes precedence over viewport movement.

the offset stays with the scroll widget while reactive updates preserve its
identity, including while it is hidden, and is clamped when its content shrinks.
a successful configuration reload and closing a popup reset their offsets.
scrollbars, horizontal scrolling, touch dragging, and programmatic movement are
not available yet.

slider defaults are 0–1, value 0, and step 0.01. a step of zero means continuous
input. the range must be finite and increasing. pointer input clamps to the range;
the endpoints remain reachable when the range is not a multiple of the step.

for live lists, create widgets in a `children` function:

```luau
ui.column {
    children = function()
        local rows = {}
        for _, device in services.audio.devices:get() do
            table.insert(rows, ui.button {
                key = device.name,
                on_click = function() services.audio.set_default_sink(device.name) end,
                children = { ui.text { text = device.description } },
            })
        end
        return rows
    end,
}
```

list updates replace widgets and handlers together after validation. invalid
updates keep the current list. removed rows unsubscribe from state. create local
state outside child functions; keys do not create per-row state.

click handlers receive no arguments. `on_scroll(x, y)` receives scroll steps;
positive values move right/down. high-resolution wheels retain fractional steps.
for continuous devices without wheel-step data, 15 compositor-reported pixels
count as one step. a scroll gesture does not open a grabbed popup. buttons remain
keyboard-activatable through the primary `on_click` handler.

`on_hover(inside)` receives `true` when the pointer enters a button, slider, or
popup and `false` when it leaves. moving between a control and its descendants
does not produce a false leave. the callback runs on edges, not on every pointer
motion.

## images and desktop icons

```luau
ui.image {
    app_id = 'firefox',
    icon_theme = 'Papirus-Dark',
    style = { width = 14, height = 14 },
}
```

use `source` for a local file, `icon` for a theme icon name, or `app_id` to look up
an application's desktop-entry icon. these properties are mutually exclusive.
`icon_theme` defaults to `hicolor`. application matching uses desktop filenames
and `StartupWMClass`, case-insensitively, then the app id itself. missing icons
fall back to `application-x-executable`. application discovery honors xdg data
directories; icon resolution honors theme inheritance through freedesktop-icons.

file paths currently resolve against the process working directory; absolute
paths avoid ambiguity. png, jpeg, webp, and path-based svg artwork are supported.
svg text, embedded images, external references, animation, and svgz are not
supported. images fit inside their content box without changing aspect ratio;
the default box is 32×32 logical pixels. svgs rasterize at the output scale.

files are limited to 16 mib, raster input dimensions to 8192 per axis, and
rendered images to 4096 per axis. the decoder allocation limit is 64 mib; the
texture cache holds up to 128 entries and 64 mib of pixel data. failed loads report an error and leave an empty image
box. reload clears the cache; changes to image files alone do not trigger reload.

## state and property functions

`ui.state(initial)` creates a value with `get()` and `set(value)` methods. state
can hold a finite number, utf-8 string, boolean, or `nil`. it cannot hold a table;
changing a table in place would bypass update tracking. a configuration can create
at most 10,000 state values.

use a function when a property should follow state:

```luau
local volume = ui.state(0.5)
local slider = ui.slider {
    value = function() return volume:get() end,
    on_change = function(value) volume:set(value) end,
}
```

reading `volume:get()` directly while building a widget captures its current value.
reading it inside a property function subscribes that property to changes. reads
inside conditional branches update those subscriptions on each evaluation.
writing the same value does not trigger an update.

property functions must not change or create state. create state during setup and
change it in event handlers. callbacks may mutate state before throwing an error;
those state changes are not rolled back. widget updates are validated together.
an invalid update leaves the current widgets unchanged and retries when another
tracked dependency changes.

## niri workspaces and windows

`services.niri` provides niri workspace and window state. other widgets and services
work without niri; `available:get()` is false when no niri connection exists.
`error:get()` reports connection failures or the latest rejected action.

`workspaces:get()` returns an immutable array sorted by output and workspace
index. entries contain `id`, `index`, `name`, `output`, `active`, `focused`, and
`urgent`. a workspace's index can change; use its `id` as a widget key.

`windows:get()` returns an immutable array sorted by id. entries contain `id`,
`workspace_id`, `title`, `app_id`, `focused`, `urgent`, `floating`, `column`, `row`,
`width`, and `height`. column and row are one-based and can be nil; width and height
are the tile dimensions in logical pixels. optional strings and workspace ids
can also be nil. workspaces and windows may briefly refer to different compositor
states while related events arrive; scripts must tolerate missing references.

ids are decimal **strings**, preserving all 64 bits. pass them unchanged to
`focus_workspace(id)`, `focus_window(id)`, or `close_window(id)` in event handlers.
actions run in order; displayed state follows niri's event stream. closing a window asks the
application to close normally; it does not kill its process.

the adapter uses `$NIRI_SOCKET`, reconnects after a disconnection, and clears its
lists while offline. disconnection discards pending commands so they cannot run
unexpectedly after a reconnect. an action already sent may have taken effect
even if its acknowledgement was lost. reloading scripts preserves service data.

## styles and themes

put layout and paint values in `style`. dimensions use logical pixels.

| property | default |
| --- | --- |
| `width`, `height` | unspecified |
| `padding`, `gap`, `grow`, `radius` | `0` |
| `border_width`, `border_color` | `0`, transparent; border paints inside the box |
| `padding_x`, `padding_y` | unspecified; override `padding` on that axis |
| `font_size` | `14` |
| `font_weight` | `400`; a number from `1` to `1000`, with `700` for bold |
| `font_family` | `sans-serif`, resolved through fontconfig |
| `background` | transparent |
| `foreground` | white |
| `opacity` | `1` |

colors use `#RRGGBB` or `#RRGGBBAA`. numeric style values must be finite and between
0 and 100,000; opacity is between 0 and 1. parent opacity multiplies child opacity.
other style values do not inherit implicitly.
individual style values can be functions, just like widget properties.

a theme is an ordinary table returned from a module. for example, save this as
`theme.luau` next to your panel:

```luau
return {
    label = { font_size = 14, foreground = '#e5e9f0' },
    button = { padding = 6, radius = 4, background = '#343c50' },
}
```

load it with `local theme = require('./theme')`, then pass `style = theme.label`
to a text widget or `style = theme.button` to a button. changing the table after
construction does not update existing widgets; use property functions for style
values that need to follow state.

## imports

`require('./theme')` loads `theme.luau` relative to the importing file. `../` paths
and explicit file extensions are supported. modules must return a non-`nil` value.
each canonical file path is evaluated once per configuration load. circular imports and
more than 64 nested imports are errors. files are limited to 1 mib each.

imports may read files outside the configuration directory. treat configurations
as local code you trust; the runtime is not a security boundary for hostile code.

## errors and reloads

`thing check` exits with 0 for a valid configuration, 1 for a configuration error,
and 2 for incorrect command usage. it evaluates function-valued properties once.
an event handler that only fails when clicked still needs interaction tests.

`thing run` watches the directories of the entry file and imported modules, with
an 80 ms debounce for saves. directory watches also handle editors that replace
files atomically. if a failed edit imports a missing file in a new directory,
save the entry file again after creating that file so the runtime retries it.
a replacement is validated before it is installed. failure preserves widgets,
state, and handlers. success closes popups and resets local state. current service
values are available when the new configuration loads.
reloading and file reads currently happen on the ui thread.

each load, event callback, and reactive update batch has a default execution budget
of 250 ms. the luau heap is limited to 64 mib; this is a ceiling, not preallocated
memory or a limit on the whole application. the execution budget does not bound native i/o time.

## clock and audio

service values have the same `:get()` interface as local state, but are read-only.
read them inside property functions to subscribe to updates.

| value | meaning |
| --- | --- |
| `services.clock.time` | local `HH:MM` time, updated at minute boundaries |
| `services.clock.date` | local date such as `Wed 28 Oct`, updated with the clock |
| `services.audio.available` | whether a default output is available |
| `services.audio.volume` | default output volume; `1` is 100%, server values may exceed it |
| `services.audio.muted` | default output mute state |
| `services.audio.device` | default output's server name |
| `services.audio.devices` | immutable array of `{ name, description }` records |
| `services.audio.source_available` | whether a default input is available |
| `services.audio.source_volume` | default input volume; `1` is 100%, server values may exceed it |
| `services.audio.source_muted` | default input mute state |
| `services.audio.source_device` | default input's server name |
| `services.audio.sources` | immutable array of selectable `{ name, description }` input records |
| `services.audio.error` | connection failure message, or an empty string |

use actions in event handlers to change audio:

```luau
local audio = services.audio
return ui.panel {
    children = {
        ui.slider {
            enabled = function() return audio.available:get() end,
            value = function() return math.min(audio.volume:get(), 1) end,
            on_change = function(value) audio.set_volume(value) end,
        },
    },
}
```

`set_volume(number)` accepts 0–1; `set_muted(boolean)` sets mute; and
`set_default_sink(name)` selects an output. the corresponding input actions are
`set_source_volume(number)`, `set_source_muted(boolean)`, and
`set_default_source(name)`. selecting a device changes the server default;
moving existing playback or recording streams depends on the audio server's
policy.

`sources` leaves out the monitor devices that audio servers create for recording
an output. if a monitor is already the default input, it remains in the list so
the current selection is still visible.

actions are forbidden during load and property evaluation. a failed event or
reactive update discards actions queued by that event. local state changes are
not rolled back. displayed values follow server replies.

volume changes preserve channel balance. the service reconnects after a server
failure and discards commands from the old connection.
`thing check` uses unavailable/empty service defaults and never connects to audio.

## popups and input

place a popup under its trigger button and bind `visible` to local state. change
that state in `on_click` and reset it in `on_close`:

```luau
local open = ui.state(false)

return ui.panel {
    style = { height = 36, padding = 6, background = '#191b22' },
    children = {
        ui.button {
            on_click = function() open:set(not open:get()) end,
            children = {
                ui.text { text = 'open popup' },
                ui.popup {
                    visible = function() return open:get() end,
                    on_close = function() open:set(false) end,
                    style = { width = 200, height = 80, padding = 12, background = '#343c50' },
                    children = { ui.text { text = 'hello' } },
                },
            },
        },
    },
}
```

the host anchors the popup to its nearest button ancestor and asks the compositor
to constrain its position to the output.

for a popup that opens on hover, let the host manage the delay and the crossing
between the button and popup surfaces:

```luau
ui.button {
    children = {
        ui.text { text = 'audio' },
        ui.popup {
            open_on_hover = true,
            open_on_click = true,
            open_delay = 300,
            close_delay = 250,
            style = { width = 240, height = 160 },
            children = { ui.text { text = 'mixer' } },
        },
    },
}
```

`open_on_hover` opens an ungrabbed popup while the pointer is over its button or
the popup itself. keyboard focus on the button also opens it. `open_delay` and
`close_delay` are whole milliseconds from 0 to 60,000 and default to 300 and
250. `open_on_click` additionally toggles it as a grabbed popup on a primary click
or return/space. it does not replace or repeat the button's `on_click` handler.

an automatic popup's `visible` property is an availability gate. setting it to
false cancels a pending open and closes the popup. one button can own one
automatic popup. `on_close` runs when the popup is dismissed, but not when an
open hover popup is recreated in grabbed mode.

the host supports one grabbed popup at a time. an input event is required to open
it. outside clicks and escape dismiss it and call `on_close`. tab/shift+tab move
focus; return/space activate a focused button. slider arrows adjust by `step`
(or 1% of the range when step is zero); home/end reach the endpoints. key repeat
currently depends on the compositor; thing does not generate repeated presses.
