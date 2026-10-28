# configuration

a configuration is a utf-8 `.luau` file that returns one `ui.panel` or a dense
array of panels. type annotations are accepted, but `thing check` does not
perform static type checking.

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

thing runs a separate configuration for each connected output. all surfaces
returned for one output share local state; another output gets its own runtime
and state. desktop services such as audio still describe the same system.

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

## multiple surfaces

return an array when one output needs independent surfaces, such as a bar and a
drawer. every panel in the array needs a unique, static `key`:

```luau
local drawer_open = ui.state(false)

return {
    ui.panel {
        key = 'bar',
        style = { height = 36, padding = 6, background = '#191b22' },
        children = {
            ui.button {
                on_click = function() drawer_open:set(not drawer_open:get()) end,
                children = { ui.text { text = 'menu' } },
            },
        },
    },
    ui.panel {
        key = 'drawer',
        layer = 'overlay',
        anchors = { top = true, right = true },
        visible = function() return drawer_open:get() end,
        style = { width = 320, height = 480, background = '#252936' },
    },
}
```

the two panels use the same `drawer_open` value because they belong to one
output runtime. hiding a root destroys its wayland surface but keeps the runtime
and its state; showing it creates the surface again. the array itself is chosen
when the configuration loads and changes only on reload. returning an empty
array is valid, which is useful when a configuration should skip an output.

`key` identifies a surface across reactive updates. `namespace` sets its
layer-shell namespace and is also static; it defaults to `thing-<key>`. the
single-panel form keeps the `thing` namespace when neither value is set. keys
and explicit namespaces must be nonempty strings of at most 256 bytes.

an output may have at most 64 surfaces. ordering between surfaces on the same
compositor layer is unspecified, so use different layers when their stacking
order matters. popups remain attached to the root that contains them.

## panel surface

surface properties belong on the root `ui.panel`.

| property | default | meaning |
| --- | --- | --- |
| `visible` | `true` | whether the surface is mapped |
| `edge` | `'top'` | shorthand for a bar on `'top'`, `'bottom'`, `'left'`, or `'right'` |
| `anchors` | absent | explicit `{ top, right, bottom, left }` output-edge anchors; cannot be combined with `edge` |
| `layer` | `'top'` | `'background'`, `'bottom'`, `'top'`, or `'overlay'` |
| `keyboard_mode` | `'on_demand'` | `'none'`, `'on_demand'`, or `'exclusive'` |
| `margins` | all zero | `{ top, right, bottom, left }` distances from the output edges |
| `exclusive_zone` | automatic | workspace space reserved for the surface; `0` reserves nothing and `-1` ignores other exclusive zones |
| `input_region` | whole surface | pointer and touch input rectangles, or `false` for click-through |

`edge` is the convenient form for bars. a top or bottom bar fills the output
width unless `style.width` is set; its height defaults to 36 logical pixels. a
left or right bar follows the same rule with the axes exchanged. a fixed bar is
centered along its edge.

use `anchors` for other layer surfaces. an omitted width requires both `left`
and `right`; an omitted height requires both `top` and `bottom`. an empty anchor
table is valid when both dimensions are fixed and lets the compositor center the
surface. explicit anchors reserve no space by default. a positive
`exclusive_zone` requires one unambiguous owning edge, such as
`{ top = true, left = true, right = true }`.

`keyboard_mode = 'none'` leaves keyboard focus with ordinary applications.
`'on_demand'` lets the compositor focus the surface after interaction, while
`'exclusive'` requests focus whenever it is mapped. on-demand mode requires
wlr-layer-shell version 4.

automatic exclusivity follows the configured surface size. it includes the
margin opposite the panel edge; the compositor also includes the margin on the
panel edge. an explicit `exclusive_zone` is used unchanged.

the painted surface, exclusive zone, and interactive area can have different
sizes. this example paints a 52 pixel-high top panel while reserving and
accepting pointer input in only the first 36 pixels:

```luau
return ui.panel {
    exclusive_zone = 36,
    input_region = { height = 36 },
    style = { height = 52, background = '#191b22' },
}
```

input rectangles use surface-local logical pixels and are clipped to the
surface. `x` and `y` default to zero. an omitted width or height extends from
that origin to the corresponding far edge, so it follows compositor resizes.
use the `rectangles` field to combine regions:

```luau
input_region = {
    rectangles = {
        { x = 0, y = 0, width = 120, height = 36 },
        { x = 240, y = 0, width = 120, height = 36 },
    },
}
```

a bare empty table retains the older meaning of one full-surface rectangle. use
`false` for a click-through surface. an empty `rectangles` array also means no
input, which makes `{ rectangles = regions }` suitable when a property function
builds a list that may be empty. the region affects pointer and touch input, not
keyboard focus.

surface properties other than `key` and `namespace` may be functions just like
other reactive properties. the function returns the whole value: for example,
a reactive `margins` property returns the complete margins table.

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
creates a fresh widget. unknown properties are errors. each tree is limited to 64
levels. one output may contain at most 10,000 widgets across all its surfaces.

every widget accepts `visible`, `enabled`, and an optional string `key`. a hidden
or disabled ancestor also blocks input to its descendants. keys must be unique
among siblings. function-valued children preserve identity by sibling key and
widget kind; unkeyed children match by position and kind.

| widget | additional properties |
| --- | --- |
| `text` | `text`: utf-8 string; optional `max_lines` and `overflow` |
| `image` | one of `source`, `icon`, `app_id`; optional `icon_theme` and `fit` |
| `button` | `children`, `on_click`, `on_middle_click`, `on_secondary_click`, `on_scroll`, `on_hover` |
| `slider` | `min`, `max`, `value`, `step`, `on_change`, `on_hover` |
| `popup` | `visible`, `open_on_hover`, `open_on_click`, `open_delay`, `close_delay`, `anchor`, `alignment`, `offset`, `constraints`, `on_hover`, `on_close` |
| containers | `children`: array of widgets, or a function returning one |

use a positive `max_lines` to limit a text widget. omitted lines are clipped by
default. `overflow = 'ellipsis'` adds an ellipsis, but currently supports only
`max_lines = 1`:

```luau
ui.text {
    text = 'a long device description',
    max_lines = 1,
    overflow = 'ellipsis',
    style = { width = 96 },
}
```

`centerbox` takes exactly three non-popup children: start, center, and end. give
it an explicit height and width (or `grow = 1` inside a row). the middle child
stays centered even when the side groups have different widths. hidden children
keep their slot. groups can overlap if they exceed the available space; choose
which content to hide in your configuration on narrow outputs. `align` and
`align_self` still control their cross-axis placement, but `justify` and `wrap`
are not available because centerbox owns the main-axis positions.

`stack` takes its children out of normal flow and paints them in order at the
same layer position. use `align` and `justify` on the stack, or `align_self` on a
child, to place overlays within it. min and max dimensions still constrain each
overlay. stack children never wrap.

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
not available yet. scroll children stay in one column, so `wrap` is not available.
when centered or end-justified content becomes taller than its viewport, it
falls back to the start edge so the beginning remains reachable.

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
    source = '/absolute/path/to/cover.png',
    fit = 'cover',
    style = { width = 96, height = 64, radius = 6 },
}
```

use `source` for a local file, `icon` for a theme icon name, or `app_id` to look up
an application's desktop-entry icon. these properties are mutually exclusive.
`icon_theme` defaults to `hicolor`. application matching uses desktop filenames
and `StartupWMClass`, case-insensitively, then the app id itself. missing icons
fall back to `application-x-executable`. application discovery honors xdg data
directories; icon resolution honors theme inheritance through freedesktop-icons.

use `ui.asset` for artwork stored beside a configuration or reusable module:

```luau
ui.image {
    source = ui.asset('./corner.svg'),
    style = { width = 14, height = 14 },
}
```

`ui.asset` accepts a `./` or `../` path and returns an absolute path resolved
against the module where the helper came from. a helper saved in a local variable
keeps that module location when a property function calls it later. the asset does
not need to exist while the configuration loads; a missing or invalid image still
reports an error and leaves an empty image box. source strings not passed through
`ui.asset` keep their existing meaning, including resolving relative paths against
the process working directory.

png, jpeg, webp, and path-based svg artwork are supported.
svg text, embedded images, external references, animation, and svgz are not
supported. `fit` defaults to `contain`, which centers the complete image without
changing its aspect ratio. `cover` preserves the aspect ratio while cropping the
center to fill the content box. `fill` stretches the image to fill both axes.
`style.radius` rounds the visible artwork inside any padding. the default image
box is 32×32 logical pixels. svgs rasterize at the output scale.

files are limited to 16 mib, raster input dimensions to 8192 per axis, and
rendered images to 4096 per axis. the decoder allocation limit is 64 mib; the
texture cache holds up to 128 entries and 64 mib of pixel data. asset paths are
limited to 4096 bytes and each output configuration may reference 10,000 distinct
asset files. thing watches files registered by `ui.asset`; creating or changing one
reloads the configuration and clears the image cache. other source paths are not
watched.

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
those state changes are not rolled back. within one output, widget updates are
validated together. an invalid update leaves that output's current widgets
unchanged and retries when another tracked dependency changes.

## launching applications

call `services.launch` from an event handler with the program and its arguments
as an array:

```luau
ui.button {
    on_click = function()
        services.launch({ 'pavucontrol' })
    end,
}
```

arguments are passed directly to the program. thing does not split a command
string or interpret quotes, variables, pipes, redirects, or globs. if a command
really needs a shell, invoke it explicitly, such as
`services.launch({ 'sh', '-c', 'your command' })`.

the array may contain up to 256 utf-8 strings, each no longer than 4096 bytes
and together no longer than 64 kib. the program name must not be empty. the
optional working directory has the same per-string limit.

an optional second argument sets the working directory. without one, the child
inherits thing's working directory:

```luau
services.launch({ 'foot', '--working-directory', project }, project)
```

the child inherits the desktop session environment, including variables such as
`WAYLAND_DISPLAY`, `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, and `PATH`, along with
the current locale and input-method settings. for launches caused directly by a
mouse button release or key press, thing asks the compositor for a fresh xdg
activation token. it launches without one when the protocol is unavailable or
the request times out, and the compositor always decides whether the new window
receives focus.

the child's standard input, output, and error go to `/dev/null`. thing does not
expose its exit status or terminate it during a reload or normal shutdown. a
service manager can still apply its own process and cgroup lifetime policy.
launch failures are reported in the terminal running thing.

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
| `min_width`, `max_width`, `min_height`, `max_height` | unspecified |
| `padding`, `gap`, `grow`, `radius` | `0` |
| `shrink` | unspecified; `1` for scroll and `0` elsewhere |
| `align` | `center` |
| `align_self` | unspecified; inherits the parent's `align` |
| `align_content` | `start` |
| `justify` | `start` |
| `wrap` | `false` |
| `border_width`, `border_color` | `0`, transparent; border paints inside the box |
| `padding_x`, `padding_y` | unspecified; override `padding` on that axis |
| `hover_overlay`, `pressed_overlay` | `#ffffff14`, `#ffffff28`; interactive control feedback |
| `disabled_opacity` | `0.45`; fades a disabled widget and its descendants once |
| `focus_ring_color`, `focus_ring_width` | `#89b4fa`, `2`; keyboard focus indicator |
| `font_size` | `14` |
| `font_weight` | `400`; a number from `1` to `1000`, with `700` for bold |
| `font_family` | `sans-serif`, resolved through fontconfig |
| `text_align` | `start`; also accepts `center` or `end` |
| `tabular_digits` | `false`; requests equal-width digits from fonts that support them |
| `background` | transparent |
| `foreground` | white |
| `opacity` | `1` |

colors use `#RRGGBB` or `#RRGGBBAA`. numeric style values must be finite and between
0 and 100,000; opacity and disabled opacity are between 0 and 1. parent opacity
multiplies child opacity. a disabled ancestor applies its disabled opacity to the
whole branch without compounding it at every descendant.
`start` and `end` text alignment follow the text direction. other style values do
not inherit implicitly.
individual style values can be functions, just like widget properties.

rows, panels, popups, buttons, and stacks have a horizontal main axis and a
vertical cross axis. columns and scrolls swap those axes. `justify` places or
distributes children on the main axis and accepts `start`, `center`, `end`,
`space_between`, `space_around`, or `space_evenly`. `align` places children on
the cross axis and accepts `start`, `center`, `end`, or `stretch`; `align_self`
overrides it for one child.

set `wrap = true` on a row or column to move children onto another line when the
main axis fills. it is also available on panels, popups, and buttons. `gap`
separates both children and wrapped lines. `align_content` places the complete
set of lines on the cross axis using `start`, `center`, `end`, or `stretch`,
without changing how `align` places children within each line. wrapping moves
widgets; text still wraps according to the width available to its own widget.

`grow` shares spare main-axis space, while `shrink` chooses how an item gives up
space when its row or column is too small. larger values take a larger share. an
explicit positive `shrink` may reduce an item below its intrinsic content size;
set a min dimension when it needs a floor. min and max dimensions also clamp an
explicit size and the result of `grow`.

min and max dimensions are for widgets inside a surface. they are rejected on a
root panel or popup because wayland surfaces are either given an exact size or
filled by the compositor. use `width` and `height` for the surface, then put the
constraint on an inner container.

set an overlay color to transparent or the focus ring width to `0` to remove that
part of the built-in interaction appearance. for example, this keeps only a thin
monochrome keyboard indicator:

```luau
style = {
    hover_overlay = '#00000000',
    pressed_overlay = '#00000000',
    disabled_opacity = 0.25,
    focus_ring_color = '#ffffff',
    focus_ring_width = 1,
}
```

### transitions

`ui.transition` follows a numeric property function and moves smoothly to each
new result:

```luau
local expanded = ui.state(false)

ui.button {
    on_click = function() expanded:set(not expanded:get()) end,
    style = {
        width = ui.transition(function()
            return expanded:get() and 240 or 80
        end, { duration = 180, easing = 'out_cubic' }),
        opacity = ui.transition(function()
            return expanded:get() and 1 or 0.6
        end, { duration = 120 }),
    },
}
```

`duration` is a required whole number of milliseconds from 0 to 60,000. zero
changes the value immediately. `easing` defaults to `out_cubic`; the other choices
are `linear`, `in_cubic`, and `in_out_cubic`.

transitions are available for `width`, `height`, `padding`, `padding_x`,
`padding_y`, `gap`, `grow`, `radius`, `border_width`, and `opacity`. their property
functions must return finite numbers, including for normally optional dimensions;
they cannot move to or from an unspecified value. geometry changes go through the
normal layout and hit testing paths.

changing direction partway through starts from the transition's current value.
the first configuration load and a successful reload start at the function's
result without animating. a failed reload leaves an existing transition running.
visibility changes remain immediate, so hiding a widget does not create an exit
fade.

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
| `services.audio.streams` | immutable array of active output playback streams |
| `services.audio.source_available` | whether a default input is available |
| `services.audio.source_volume` | default input volume; `1` is 100%, server values may exceed it |
| `services.audio.source_muted` | default input mute state |
| `services.audio.source_device` | default input's server name |
| `services.audio.sources` | immutable array of selectable `{ name, description }` input records |
| `services.audio.all_sources` | complete immutable array of `{ name, description, monitor }` input records |
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

each item from `streams:get()` contains `id`, `application`, `app_id`, `name`,
`icon`, `volume`, `muted`, and `volume_writable`. these are output playback
streams; recording and capture streams are not included. `application` uses the
server's application name, then its process binary, then the stream name.
`app_id` uses the application id or process binary, and `name` uses the media
name or stream name. `icon` is a theme icon name suitable for `ui.image`.
metadata with no available value is an empty string. `volume` uses `1` for
100%; server values may exceed it.

the stream `id` is an opaque string for one stream lifetime. use it as a widget
key and pass it back unchanged from an event handler:

```luau
ui.column {
    children = function()
        local rows = {}
        for _, stream in services.audio.streams:get() do
            table.insert(rows, ui.button {
                key = stream.id,
                on_click = function()
                    services.audio.set_stream_muted(stream.id, not stream.muted)
                end,
                children = { ui.text { text = stream.application } },
            })
        end
        return rows
    end,
}
```

`set_stream_volume(id, number)` accepts 0–1 and preserves channel balance.
`set_stream_muted(id, boolean)` changes mute. check `volume_writable` before
showing an enabled volume control; streams without writable volume remain in the
list. ids change when a stream disappears or the server reconnects. the audio
backend ignores ids absent from its latest confirmed snapshot.

`sources` is a microphone-picker convenience. it leaves out the monitor devices
that audio servers create for recording an output, unless a monitor is already
the default input. `all_sources` is the unfiltered list; its `monitor` field lets
a configuration expose desktop-audio monitors or apply a different filter.
`set_default_source` accepts names from either list.

actions are forbidden during load and property evaluation. a failed event or
reactive update discards actions queued by that event. local state changes are
not rolled back. displayed values follow server replies.

volume changes preserve channel balance. the service reconnects after a server
failure and discards commands from the old connection.
`thing check` uses unavailable/empty service defaults and never connects to audio,
media players, or the system power service.

## media

`services.media` follows players that implement mpris, including browsers and
most desktop music applications. `players:get()` returns an immutable array so
the configuration can choose which players to show and how to arrange them.
each record contains:

| field | meaning |
| --- | --- |
| `id` | opaque id for this ownership lifetime |
| `name` | mpris instance name on the session bus |
| `playing` | whether this player reports that it is playing |
| `title`, `artist`, `art_url` | current track metadata |
| `identity`, `desktop_entry` | application identity supplied by the player |
| `can_play_pause`, `can_next`, `can_previous`, `can_stop` | available controls for this player |

controls are event-handler actions. check the matching capability and pass the
record's id to target that player:

```luau
local media = services.media

local function player_row(player)
    return ui.button {
        key = player.id,
        enabled = player.can_play_pause,
        on_click = function() media.play_pause(player.id) end,
        children = { ui.text { text = player.title } },
    }
end

ui.column {
    children = function()
        local rows = {}
        for _, player in media.players:get() do
            table.insert(rows, player_row(player))
        end
        return rows
    end,
}
```

targeting a player does not change the shared selection. playback events may
change it later. ids are deliberately opaque: use them unchanged as widget keys
and action arguments, and do not keep one after its player disappears. a player
receives a new id whenever it gives up and reacquires its bus name, including
after the session bus restarts.

for a compact, single-player widget, `available` says whether the service has a
selection and `selected_id` contains its id. the `playing`, `title`, `artist`,
`art_url`, `identity`, `desktop_entry`, `can_play_pause`, `can_next`,
`can_previous`, and `can_stop` signals mirror that record. `error` contains a
connection failure message, or an empty string. the service selects the player
that most recently began playing. while it is paused and another player is still
playing, the active player takes its place. when nothing is playing, the player
that most recently began playing is selected again.

calling `play_pause()`, `next()`, `previous()`, or `stop()` without an id controls
that selection. passing `nil` has the same effect. an explicit id never changes
the selection by itself.

an empty, malformed, or non-string id fails inside the event handler. a
well-formed id that is no longer in the current player list reaches the media
service, which rejects it and reports the error in the terminal.

state changes come from player events rather than assumptions about an accepted
action. the service tracks players joining and leaving the session bus,
reconnects after the bus is restarted, and does not replay commands across a
disconnection.

`art_url` is mpris metadata, not an image path. it may contain a `file:` url or a
remote url. `ui.image` currently accepts local filesystem paths and does not fetch
urls, so passing `art_url` directly to `source` is not supported. playback
position, seeking, and remote artwork loading are not part of this api yet.

## power

`services.power` reads battery and line-power state from UPower. `available`
says whether the service has a current snapshot, `on_battery` reports the
machine-wide power source, and `error` contains a connection failure message or
an empty string. these values are read-only signals, so use `:get()` when a
property should update with them.

`display:get()` returns UPower's composite display device, or `nil` when no
display device is available. this is usually the right record for a single
battery indicator. `devices:get()` returns an immutable array of the individual
power devices and leaves the composite record out. a configuration can use this
list for systems with several batteries, an uninterruptible power supply, or
battery-powered peripherals.

each device record contains:

| field | meaning |
| --- | --- |
| `id` | UPower object path; suitable for a widget key while the device exists |
| `kind`, `state` | snake-case device kind and charge state, or `unknown` |
| `power_supply` | whether the device supplies power to the machine |
| `present`, `online` | physical presence or line-power status when applicable |
| `percentage`, `capacity` | charge and battery health percentages when reported |
| `time_to_empty`, `time_to_full` | remaining seconds, or `nil` when UPower reports no estimate |
| `energy`, `energy_empty`, `energy_full`, `energy_full_design`, `energy_rate` | energy measurements in Wh and rate in W when applicable |
| `native_path`, `vendor`, `model`, `serial`, `icon_name` | optional device metadata |
| `warning_level`, `battery_level` | snake-case warning and coarse battery levels |

optional fields use `nil` when a value does not apply or was not supplied.
numeric zero remains a real value for percentages and energy measurements;
UPower defines zero time estimates as unknown, so those become `nil`.
the service follows devices appearing and disappearing, replaces its state when
UPower restarts, and does not provide power-management actions.

## notifications

thing can provide the standard desktop notification service. it is opt-in so it
doesn't unexpectedly replace the notification daemon you already use:

```sh
thing run --notification-server ./panel.luau
```

if another process owns `org.freedesktop.Notifications`, thing leaves it alone
and reports the conflict through `services.notifications.error`. it will not try
to take ownership later in the same run, even if that process exits. restart
thing when you want it to try again.

`services.notifications.owned:get()` says whether thing currently owns the
service. `active:get()` returns an immutable array ordered with the newest
notification lifetime first. each record contains:

| field | meaning |
| --- | --- |
| `key` | opaque string for this notification lifetime |
| `id` | freedesktop protocol id; useful for display, not as a widget key |
| `revision` | starts at 1 and increases when a client replaces the notification |
| `app_name`, `app_icon`, `summary`, `body` | content supplied by the client |
| `actions` | immutable `{ key, label }` records advertised by the client |
| `urgency`, `urgency_code` | `low`, `normal`, or `critical`, and its numeric code |
| `raw_expire_timeout_ms` | timeout from the client; `-1` means the server default |
| `effective_timeout_ms`, `expires_at_ms` | resolved timeout and server-relative deadline, or `nil` |
| `created_at_ms`, `updated_at_ms` | server-relative times for the first and latest revision |
| `category`, `desktop_entry`, `image_path` | optional application metadata |
| `resident`, `transient`, `action_icons` | standard notification behavior hints |

use the opaque `key` unchanged as the widget key and when sending an action:

```luau
local notifications = services.notifications

ui.column {
    children = function()
        local rows = {}
        for _, notification in notifications.active:get() do
            table.insert(rows, ui.button {
                key = notification.key,
                on_click = function()
                    notifications.dismiss(notification.key)
                end,
                children = {
                    ui.text { text = notification.summary },
                    ui.text { text = notification.body },
                },
            })
        end
        return rows
    end,
}
```

`invoke(key, action_key)` activates one of the record's actions. non-resident
notifications close after an action; resident notifications stay active. both
`dismiss` and `invoke` are event-handler actions and reject stale keys.

a timeout of zero never expires. the default timeout is five seconds, while
critical notifications remain until they are closed. thing does not retain
notification history. the accepted active record payload is limited to 512 kib
per record and 8 mib in total. the service reconnects after a session-bus
failure, discards queued actions from the old connection, and never reuses an
opaque key during the same run.

without `--notification-server`, the service remains unowned and empty.
`thing check` never connects to the session bus.

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

the host positions the popup from its nearest button ancestor. `anchor` chooses
the side of that button: `'top'`, `'bottom'`, `'left'`, or `'right'`. its default,
`'auto'`, opens away from the panel's output edge. `alignment` is physical: start
and end mean left and right along a top or bottom anchor, and top and bottom along
a left or right anchor. it defaults to `'end'`, preserving the original placement
for a top panel.

`offset = { x, y }` moves the result in surface-local logical pixels. both values
default to zero and may range from -100,000 to 100,000. for example, this centers
a popup below its button with an eight-pixel gap:

```luau
ui.popup {
    alignment = 'center',
    offset = { y = 8 },
    style = { width = 240, height = 160 },
}
```

the compositor keeps popups inside its own constraint boundary. by default it
may flip the popup on either axis and slide it along the alignment axis. replace
that behavior with a `constraints` table containing any of `flip_x`, `flip_y`,
`slide_x`, and `slide_y`; omitted fields are false, and an empty table disables
all adjustments. returning `nil` from a reactive `constraints` property restores
the automatic choice. the protocol does not provide a separate inset from the
constraint boundary.

placement properties and popup dimensions may be reactive. an open popup moves
after its trigger's layout changes, while a successful configuration reload
continues to close it. compositors with xdg-shell version 2 or older apply a
changed placement the next time the popup opens.

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
(or 1% of the range when step is zero); home/end reach the endpoints. thing uses
the compositor's rate and delay for its own repeat clock. if the compositor takes
over repetition, thing honors those repeated key events instead. held slider
arrows repeat. tab, escape, return/space on buttons, and slider home/end act once
per press.
