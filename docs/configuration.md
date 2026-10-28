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

## editor support

thing includes a definition file for luau-lsp. write a copy beside your
configuration, and repeat this after upgrading thing:

```sh
thing types > ./thing.d.luau
```

for the luau-lsp vscode extension, select the standard luau platform and add the
file to your workspace settings:

```json
{
    "luau-lsp.platform.type": "standard",
    "luau-lsp.types.definitionFiles": {
        "@thing": "thing.d.luau"
    }
}
```

other editors can start the server with
`--platform=standard --definitions:@thing=/path/to/thing.d.luau`. the same
options work with `luau-lsp analyze` for a command-line type check.

the definitions describe widget properties, state, and service records. this
gives editors useful completions and catches ordinary type mistakes. they
cannot express every numeric limit, placement rule, or relationship between
properties, so keep using `thing check` for thing's runtime validation.

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
| `on_key` | absent | handles keyboard presses while the surface has keyboard focus |

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
`scroll`, `text`, `text_input`, `image`, `path`, `button`, `pointer_area`,
`slider`, and `range` constructors. pass properties in a table and children in a
dense array:

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
| `text_input` | controlled `value` and `on_change`; optional `placeholder` and `on_submit` |
| `image` | one of `source`, `icon`, `app_id`; optional `icon_theme` and `fit` |
| `path` | `data`, `view_box`; optional fit, fill, and stroke controls |
| `button` | `children`, `on_click`, `on_middle_click`, `on_secondary_click`, `on_scroll`, `on_hover`, `on_pointer` |
| `pointer_area` | `children`, with at least one of `on_pointer` or `on_scroll` |
| `slider` | `min`, `max`, `value`, `step`, `on_change`, `on_hover` |
| `range` | `children`, `min`, `max`, `value`, `step`, `inset`, `on_change`, `on_hover`, `on_pressed` |
| `popup` | `visible`, `open_on_hover`, `open_on_click`, `open_delay`, `close_delay`, `anchor`, `alignment`, `offset`, `constraints`, `on_key`, `on_hover`, `on_close` |
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

`text_input` is a controlled, single-line field. keep its value in `ui.state`
and return that state from the `value` property. `on_change` receives each
proposed value; the field keeps an edit only when the handler publishes that
value back through `value`:

```luau
local query = ui.state('')

ui.text_input {
    value = function() return query:get() end,
    placeholder = 'search applications',
    on_change = function(value) query:set(value) end,
    on_submit = function(value)
        services.launch({ 'my-launcher', value })
    end,
    style = { width = 280, padding_x = 10 },
}
```

click and drag to select text, or use tab and shift-tab to move keyboard focus.
arrow keys follow visual text order, including mixed right-to-left text. shift
extends the selection; control adds word movement or deletion, and control-a
selects everything. enter calls `on_submit`. values and placeholders are limited
to one mib and cannot contain control characters or unicode line separators.

when the compositor provides wayland's text-input-v3 protocol, the field also
accepts input methods and on-screen keyboards. composing text is shown in the
field with an underline but is not included in `value`. if composition replaces
a selection, that deletion is still proposed through `on_change`. thing reports
the edited area so the compositor can place candidate windows nearby.

this support is for ordinary, non-sensitive text. while a field is focused,
thing may share a window of up to 4,000 bytes around its selection with the
compositor and input method. `text_input` does not yet have content-purpose or
password controls, so it cannot mark a field as secret.

control-c copies selected text, and control-x copies then removes it. both require
a non-empty selection. control-v reads text asynchronously and applies it only if
the same field remains focused with the same value and selection. these shortcuts
use the standard wayland clipboard and do not require the opt-in
`services.clipboard` api described below.

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
direction, then the next event moves the outer viewport. the deepest button or
pointer area with an `on_scroll` handler receives the event first. returning
`false` allows the same event to move the presented viewport; returning `true`
or nothing consumes it.

the offset stays with the scroll widget while reactive updates preserve its
identity, including while it is hidden, and is clamped when its content shrinks.
a successful configuration reload and closing a popup reset their offsets.
`ui.reveal(widget)` can move the enclosing vertical scroll views from an event
handler. scrollbars, horizontal scrolling, and touch dragging are not available
yet. scroll children stay in one column, so `wrap` is not available.
when centered or end-justified content becomes taller than its viewport, it
falls back to the start edge so the beginning remains reachable.

slider defaults are 0–1, value 0, and step 0.01. a step of zero means continuous
input. the range must be finite and increasing. pointer input clamps to the range;
the endpoints remain reachable when the range is not a multiple of the step.

`range` provides the same value and keyboard behavior without a built-in
appearance. its ordinary visual children form a horizontal flex container. the
configuration remains responsible for updating `value` from `on_change`:

```luau
local amount = ui.state(0.4)

ui.range {
    value = function() return amount:get() end,
    inset = 6,
    on_change = function(value) amount:set(value) end,
    style = { width = 180, height = 24 },
    children = {
        ui.row {
            style = {
                width_fraction = function() return amount:get() end,
                min_width = 12,
                height = 12,
                radius = 6,
                background = '#89b4fa',
            },
        },
        ui.row {
            style = {
                width = 0,
                grow = 1,
                height = 12,
            },
        },
    },
}
```

`inset` defaults to zero and moves the two pointer endpoints inward in logical
pixels. rows, columns, center boxes, stacks, text, and images may appear below a
range. interactive widgets, popups, panels, and scroll views may not. keys on
visual children follow the same sibling identity rules as other containers.

`on_pressed(down)` reports a primary pointer drag while the current configuration
remains loaded. release, removal, hiding, disabling, or loss of pointer input
reports `false`. replacing the complete configuration discards its local state
and handlers without running teardown callbacks.

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

click handlers receive no arguments. `on_scroll(x, y, event)` receives normalized
scroll steps; positive values move right/down, and diagonal input produces one
callback. existing two-argument handlers continue to work. high-resolution
wheels retain fractional steps. for continuous devices without wheel-step data,
15 compositor-reported pixels count as one step.

`event.local_x` and `event.local_y` are logical coordinates within the handler,
while `surface_x` and `surface_y` are logical surface coordinates. `source` is
`wheel`, `finger`, `continuous`, `wheel_tilt`, or `unknown`; the remaining fields
report the `shift`, `ctrl`, `alt`, and `logo` modifiers. the event table is
read-only. a handler error discards actions queued by that callback and stops the
built-in scroll fallback. it does not undo arbitrary changes already made to
luau values. a scroll gesture does not open a grabbed popup. buttons remain
keyboard-activatable through the primary `on_click` handler.

`on_hover(inside)` receives `true` when the pointer enters a button, slider,
range, or popup and `false` when it leaves. moving between a control and its
descendants does not produce a false leave. the callback runs on edges, not on
every pointer motion.

use `pointer_area` for a drag region or gesture that has no built-in button
meaning. it is a horizontal container, does not enter keyboard focus order, and
does not draw button interaction feedback. use a button when the same action
also needs keyboard activation. a pointer area may provide `on_pointer`,
`on_scroll`, or both. a scroll-only area does not capture presses or acquire
button activation behavior.

```luau
local last_x = ui.state(0)

ui.pointer_area {
    on_pointer = function(event)
        if event.phase ~= 'cancel' then last_x:set(event.x) end
        return true
    end,
    children = {
        ui.text { text = function() return string.format('%.1f', last_x:get()) end },
    },
}
```

`on_pointer(event)` receives `press`, `move`, `release`, and `cancel` phases.
`event.x` and `event.y` are logical coordinates within the presented widget;
`surface_x` and `surface_y` are logical surface coordinates. `button` is
`primary`, `middle`, or `secondary`, and the `shift`, `ctrl`, `alt`, and `logo`
fields report the current modifiers. a press captures subsequent motion and
release even outside the widget. if layout moves during the drag, local
coordinates follow the geometry currently shown on screen. a synthetic
cancellation keeps the last delivered coordinates because its target may no
longer have geometry.

the deepest control or pointer area owns pointer input; custom events do not
bubble to ancestors and capture cannot move to another widget. hiding,
disabling, or removing the target, losing pointer input, or closing its surface
sends one `cancel` while the runtime remains loaded. a successful configuration
reload drops the old runtime without calling its handlers. returning `true`
from any phase suppresses a button's ordinary click for the rest of that
sequence; `false` or `nil` leaves it available. the release callback runs before
that ordinary click.

## images and desktop icons

```luau
ui.image {
    source = '/absolute/path/to/cover.png',
    fit = 'cover',
    style = { width = 96, height = 64, radius = 6 },
}
```

use `source` for a local file or an opaque image handle supplied by a desktop
service, `icon` for a theme icon name, or `app_id` to look up an application's
desktop-entry icon. these properties are mutually exclusive.
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

## vector paths

`ui.path` draws svg path data without loading a complete svg document. this is
useful for icons, indicators, separators, and decorative shapes that need to
follow the rest of a luau theme.

```luau
ui.path {
    data = 'M 12 2 L 22 20 H 2 Z',
    view_box = { x = 0, y = 0, width = 24, height = 24 },
    fill = '#89b4fa',
    stroke = '#11111b',
    stroke_width = 1.5,
    stroke_join = 'round',
    style = { width = 18, height = 18 },
}
```

`data` accepts the standard `M`, `L`, `H`, `V`, `C`, `S`, `Q`, `T`, `A`, and `Z`
commands, including their relative forms. `view_box` is required and supplies the
path's intrinsic size when the style does not set one. its width and height must
be positive.

`fit` defaults to `contain`; `cover` fills and clips the content box, while `fill`
allows different horizontal and vertical scales. `fill` defaults to
`style.foreground`, and `fill_rule` accepts `nonzero` or `evenodd`. strokes are
transparent by default. `stroke_cap` accepts `butt`, `round`, or `square`, and
`stroke_join` accepts `miter`, `round`, or `bevel`. stroke width stays in logical
pixels after the path is fitted, including with `fit = 'fill'`.

padding defines the content box. `style.radius` clips the drawing within that
box; the widget's background and border still use its complete bounds. a path is
a visual leaf, so interaction belongs on a surrounding button or other control.

path data is limited to 256 kib and 4,096 segments both before and after arcs and
shorthand commands are normalized. coordinates and view-box edges must stay
between -100,000 and 100,000. a rejected reactive update leaves the current tree
in place.

## state and property functions

`ui.state(initial)` creates a value with `get()` and `set(value)` methods. state
can hold a finite number, utf-8 string, boolean, or `nil`. it cannot hold a table;
changing a table in place would bypass update tracking. a configuration can create
at most 10,000 state values.

the editor definitions infer a state's type from its initial value. when a state
intentionally changes between scalar types, choose the union explicitly:

```luau
local status = ui.state<<number | string>>(0)
status:set('ready')
```

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

## managed processes

use `services.process` when a configuration needs to read a child process or
keep it within the shell's lifetime. there are two forms: a manual process that
event handlers start and stop, and a scheduled command that thing runs for you.

declare a manual process while the configuration loads, then control it from an
event handler:

```luau
local updates = services.process('updates')
local recorder = services.process {
    key = 'recorder',
    shutdown_signal = 'interrupt',
}

return ui.button {
    on_click = function()
        if updates.running:get() then
            updates.stop()
        else
            updates.start({ 'checkupdates' })
        end
    end,
    children = { ui.text { text = function() return updates.stdout:get() end } },
}
```

a key names one process across all output runtimes. repeated declarations must
match exactly; conflicting declarations reject the candidate configuration.
`terminate` is the default shutdown signal. `interrupt` is useful for a program
that treats `SIGINT` as a request to finish its current work.

the handle exposes read-only `status`, `running`, `stdout`, `stderr`,
`stdout_truncated`, `stderr_truncated`, `exit_code`, `exit_signal`, and `error`
signals. status is `idle`, `starting`, `running`, `stopping`, `exited`, or
`failed`. `running` remains true while a stop is in progress. starting again
clears output and exit state from the previous run.

`start(arguments, working_directory?)` uses the same argument and working
directory limits as `services.launch`. arguments go directly to the program;
there is no implicit shell. stdin is `/dev/null`, stdout and stderr are captured,
and desktop activation variables are removed. use `services.launch` for ordinary
applications that should receive compositor activation and outlive the panel
configuration.

for a finite query that should run at startup or on a regular cadence, put its
command in the declaration:

```luau
local weather = services.process {
    key = 'weather',
    command = {
        'curl', '--fail', '--silent', '--max-time', '30',
        'https://wttr.in/?format=1',
    },
    interval = 15 * 60 * 1000,
}

return ui.row {
    children = {
        ui.text { text = function() return weather.stdout:get() end },
        ui.button {
            on_click = function() weather.refresh() end,
            children = { ui.text { text = 'refresh' } },
        },
    },
}
```

`command` is the same direct argument array accepted by `start`. an optional
`working_directory` applies to every run. `interval` is a whole number of
milliseconds from 1,000 through 31,536,000,000. without an interval, the command
runs once when its declaration becomes active.

scheduled commands are intended for finite work such as system statistics,
update counts, or weather. while a new run is active, the public output and exit
signals keep the last completed result. stdout, stderr, truncation flags, and
exit values change together only after the child has been reaped, so a widget
does not briefly see an empty or half-written sample. status, running, and
supervisor errors still describe the current run. use the manual form when live
streaming output is the point of the process.

`refresh()` asks for the declared command to run now. repeated requests
coalesce: a queued or starting run already satisfies the request, and a running
child can leave at most one follow-up run. `cancel()` cancels queued or active
work and discards its partial capture. it does not disable the declaration; a
recurring command receives a new deadline at least one interval later. durable
pause and resume are not part of this api.

the scheduler is shared by every output and uses monotonic time. there is no
overlap or catch-up burst. a late idle command runs once and starts a fresh
interval, while an ordinary deadline reached during an active run is skipped.
adding another output with the same declaration does not rerun startup work.
nonzero exits do not disable a recurring command. a spawn failure waits for the
next interval; a one-shot command stays failed until `refresh()`.

each stdout or stderr tail retains at most 64 kib, on utf-8 boundaries, and
reports when older text was discarded. manual stream updates are coalesced to
at most 20 per second, with exit and end-of-file changes delivered immediately.
one process-wide supervisor limits the shell to 32 declared keys, 32 unreaped child
incarnations, 128 queued starts, and 8 mib of retained or reserved capture
buffers across manual and scheduled work, including retiring generations.
current bounded strings are also projected into each output runtime that
declares the key; this is not a total heap limit. a key cannot start again until
its previous incarnation has been reaped.

a failed reload leaves processes, results, deadlines, and widgets alone. a
successful reload preserves an exactly matching scheduled declaration,
including its current run, last result, pending refresh, and deadline. changing
any scheduled option retires the old incarnation and activates a new one after
commit. manual processes keep their existing generation lifetime: a successful
reload retires them and gives the replacement configuration an idle handle.
adding or removing an output keeps either form alive for as long as another
output declares its key.

stopping or canceling first sends the configured signal and, after five
seconds, escalates to `SIGKILL`. thing also retires managed children during
shutdown, using a shorter grace period so ordinary killable children do not
survive the host.

ownership covers the direct child and processes that remain in its initial
process group. a descendant that deliberately creates or joins another group or
session is outside that boundary. the first implementation needs a real procfs
mounted at `/proc`, rooted at `/`, with mount identity available through
`statx` and unrestricted process visibility (`hidepid=0` or `hidepid=off`). this
is checked only when a configuration declares a managed process. if the check
fails, initial configuration reports a service error; a reload keeps the working
configuration. configurations that do not use `services.process` never start
the worker or perform this check. `thing check` validates declarations but does
not start children.

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
| `width`, `height` | unspecified; logical pixels |
| `width_fraction`, `height_fraction` | unspecified; a fraction from `0` to `1` of the parent's content size |
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
| `placeholder_color`, `selection_color`, `caret_color` | `#ffffff80`, `#89b4fa80`, white; `text_input` only |
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

rows, panels, popups, buttons, pointer areas, and stacks have a horizontal main
axis and a vertical cross axis. columns and scrolls swap those axes. `justify`
places or distributes children on the main axis and accepts `start`, `center`,
`end`, `space_between`, `space_around`, or `space_evenly`. `align` places
children on the cross axis and accepts `start`, `center`, `end`, or `stretch`;
`align_self` overrides it for one child.

set `wrap = true` on a row or column to move children onto another line when the
main axis fills. it is also available on panels, popups, buttons, and pointer
areas. `gap` separates both children and wrapped lines. `align_content` places
the complete set of lines on the cross axis using `start`, `center`, `end`, or
`stretch`, without changing how `align` places children within each line.
wrapping moves widgets; text still wraps according to the width available to
its own widget.

`grow` shares spare main-axis space, while `shrink` chooses how an item gives up
space when its row or column is too small. larger values take a larger share. an
explicit positive `shrink` may reduce an item below its intrinsic content size;
set a min dimension when it needs a floor. min and max dimensions also clamp an
explicit size and the result of `grow`.

`width_fraction` and `height_fraction` make a widget responsive to its parent.
they cannot be combined with `width` or `height` on the same axis. pixel min and
max dimensions still clamp the result.

min, max, and fractional dimensions are for widgets inside a surface. they are
rejected on a root panel or popup because wayland surfaces are either given an
exact size or filled by the compositor. use `width` and `height` for the surface,
then put the responsive or constrained dimension on an inner container.

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

### transitions and springs

`ui.transition` follows a numeric or color property function and moves smoothly
to each new result:

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
        background = ui.transition(function()
            return expanded:get() and '#89b4fa' or '#343c50'
        end, { duration = 180 }),
    },
}
```

`duration` is a required whole number of milliseconds from 0 to 60,000. zero
changes the value immediately. `easing` defaults to `out_cubic`; the other choices
are `linear`, `in_cubic`, and `in_out_cubic`.

`ui.spring` follows a numeric property without fixing the motion to a duration:

```luau
style = {
    width = ui.spring(function()
        return expanded:get() and 240 or 80
    end, {
        stiffness = 170,
        damping = 0.8,
        epsilon = 0.1,
    }),
}
```

`stiffness` and `damping` must be positive finite numbers. extreme combinations
outside the solver's representable range are rejected. stiffness controls how
quickly the value is pulled towards its target. damping is a ratio: below 1 is
underdamped and oscillatory, 1 is critically damped, and above 1 is overdamped.
`epsilon` defaults to 0.01 in the property's units. a spring stops when both its
distance from the target is at most epsilon and its speed is at most
`epsilon * sqrt(stiffness)` units per second.

numeric transitions and springs are available for `width`, `height`, `padding`,
`padding_x`, `padding_y`, `gap`, `grow`, `radius`, `border_width`, and `opacity`.
their property functions must return finite numbers, including for normally
optional dimensions; they cannot move to or from an unspecified value. geometry
changes go through the normal layout and hit testing paths.

color transitions are available for `background`, `foreground`, `border_color`,
`hover_overlay`, `pressed_overlay`, and `focus_ring_color`. their property
functions return `#RRGGBB` or `#RRGGBBAA`. rgb channels are mixed in linear srgb
after premultiplying them by alpha; alpha is mixed linearly. endpoints keep their
exact channel values.

changing direction partway through starts from the value currently being
calculated. a spring also keeps its velocity, so an interrupted movement does not
stop and restart. keyed children keep that motion when a reactive list rebuilds
them with the same spring options. removing a child cancels its motion.

spring targets still have to be valid style values. when an underdamped spring
temporarily overshoots a property's range, thing displays the nearest valid value
without discarding the spring's internal position or velocity. this lets an
opacity or size spring settle naturally instead of turning a valid target into a
configuration error.

the first configuration load and a successful reload start at the function's
result without animating. a failed reload leaves existing motion running.
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
media players, the system power service, or the session bus for notification and
tray state.

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

## system tray

tray support is opt-in. use `host` when another process already provides the
status notifier watcher:

```sh
thing run --system-tray host ./panel.luau
```

use `own-watcher` when thing should provide the watcher itself:

```sh
thing run --system-tray own-watcher ./panel.luau
```

own-watcher mode claims both the kde and freedesktop watcher names only when
both are free. it never replaces a running watcher; if either name is already
owned, thing follows the existing watcher as a host instead. without the option,
thing does not start its tray worker. `thing check` also leaves it disabled and
does not contact the session bus for tray state.

`services.system_tray` contains these read-only values:

| value | meaning |
| --- | --- |
| `available` | whether thing is attached to a watcher |
| `watcher_owned` | whether thing currently owns the watcher names |
| `error` | connection failure message, or an empty string |
| `items:get()` | immutable array of current status notifier item records |

each item has an opaque `id` for one bus-owner lifetime. use it unchanged as a
widget key and action argument. it changes when the item or selected watcher is
replaced, which prevents a delayed action from reaching a different process.
`protocol_item_id` is the application's own status notifier id; it is metadata,
not an action target and is not guaranteed to be unique.

the other string fields are `category`, `title`, `status`, `icon_name`,
`attention_icon_name`, `overlay_icon_name`, `tooltip_icon_name`,
`tooltip_title`, and `tooltip_text`. `item_is_menu` describes the item's primary
activation behavior. `has_menu` says whether the item advertised a DBusMenu;
when it is true, `menu` contains its root record. the four `has_*_icon_pixmap`
fields report whether a bounded, valid pixmap was advertised. the normal and
attention flags become false when the aggregate retention budget prevents thing
from keeping their pixel data.

`icon` is an opaque image handle ready for `ui.image.source`, or `nil` when the
item has no usable icon. thing prefers the advertised theme name and falls back
to the closest retained pixmap when the name cannot be loaded. an item in
`NeedsAttention` state tries its attention icon before the normal icon. handles
are immutable: an effective icon or status-candidate change receives a new
handle, while configuration code that still holds an older handle can continue
to render its previous candidates.

build the presentation in the configuration like any other dynamic list:

```luau
local tray = services.system_tray

ui.row {
    children = function()
        local children = {}
        for _, item in tray.items:get() do
            table.insert(children, ui.button {
                key = item.id,
                on_click = function() tray.activate(item.id) end,
                on_middle_click = function() tray.secondary_activate(item.id) end,
                on_secondary_click = function() tray.context_menu(item.id) end,
                children = { ui.image { source = item.icon } },
            })
        end
        return children
    end,
}
```

the three item actions are event-handler only. thing currently passes `(0, 0)`
as their coordinates because wayland clients do not know global screen
coordinates, and it does not expose the protocol's scroll action.

### tray menus

thing exposes DBusMenu data rather than choosing how a menu should look. a menu
root has `id`, `loaded`, `loading`, `error`, and `children`. call
`open_menu(root.id)` when your configuration begins showing it; the children
then update through `services.system_tray.items`. call `close_menu(root.id)`
when the presentation closes. these functions, and `activate_menu_item`, may
only be called from event handlers.

menu entries contain these read-only fields:

| field | meaning |
| --- | --- |
| `id` | opaque target for menu actions |
| `kind` | protocol item type, normally `standard` or `separator` |
| `label`, `access_key` | display-ready label and its optional mnemonic key |
| `enabled`, `visible` | current interaction and presentation state |
| `icon_name` | optional theme icon name |
| `shortcut` | immutable array of key-name arrays supplied by the application |
| `toggle_type`, `toggle_state` | check or radio metadata; `0` is off, `1` is on, and other states are indeterminate |
| `disposition` | protocol disposition such as `normal`, `informative`, or `warning` |
| `has_submenu` | whether the entry can be opened as a submenu |
| `loaded`, `loading`, `error`, `children` | lazy child state and the last valid child records |

open a submenu with its own id just before revealing it. `open_menu` is
idempotent for one output configuration, and separate outputs may hold the same
branch open at once. `close_menu` releases the branch and any descendants for
the calling configuration. a successful reload or a removed output releases
its outstanding opens automatically. the lease belongs to the output
configuration, not an individual widget. keep at most one live presentation of
a branch in that configuration, call `open_menu` once when it appears, and call
`close_menu` once when it disappears. repeated opens do not create separate
leases. this matches thing's current one-grabbed-popup-per-output model.

activate a visible, enabled, non-separator entry with
`activate_menu_item(entry.id)`. ids are opaque and may change when an item,
menu path, parent, or bus connection changes; use the current records rather
than constructing or saving ids. stale actions are rejected.

`loading` can be true while the previous children remain available. a failed
refresh keeps those children and puts a bounded message in `error`, so a menu
does not disappear because one request failed. labels already have DBusMenu's
underscore mnemonic syntax removed; `access_key` preserves the first mnemonic.

the menu transport retains at most 64 direct children, 16 levels and 256 nodes
per menu. its cache is capped at 512 kib per menu, 2,048 nodes and 8 mib across
one tray worker. the immutable snapshot is checked against the same limits and
shares one menu-tree allocation across output runtimes instead of copying it for
each output. closed cache branches are discarded least-recently-used when space
is needed; open or loading branches are not evicted.

the first menu API does not retain `icon-data` or apply `IconThemePath`, and it
does not expose vendor properties, hover events, or grouped events. applications
that send `ItemActivationRequested` cannot make thing open a particular custom
presentation yet. thing also does not provide a menu widget, typeahead,
menu-specific keyboard navigation, or an accessibility implementation; those
remain responsibilities or future capabilities of the configuration layer.

pixmap payloads are limited to 512 kib per item and 32 mib across the tray.
equal-size animation frames are not animated, overlay and tooltip pixmaps are
not composited, and legacy XEmbed tray items are not supported.

## clipboard

clipboard access is opt-in because it lets a configuration read text copied in
other applications, even when a thing surface is not focused. enable it only for
a configuration you trust:

```sh
thing run --clipboard ./panel.luau
```

`services.clipboard.text:get()` contains the current regular clipboard text, or
`nil` when the selection is empty or does not offer text. `loading:get()` is true
while thing is reading a new selection. `available:get()` says whether the
compositor has provided a usable clipboard device, and `error:get()` contains a
protocol or read error when something goes wrong. outgoing write failures are
reported in the terminal running thing.

these are reactive values. this button follows the current text and replaces it
when clicked:

```luau
local clipboard = services.clipboard

ui.button {
    enabled = function() return clipboard.available:get() end,
    on_click = function() clipboard.set_text('copied from thing') end,
    children = {
        ui.text {
            text = function()
                if clipboard.loading:get() then return 'reading…' end
                return clipboard.text:get() or 'clipboard has no text'
            end,
        },
    },
}
```

`set_text(text)` and `clear()` are event-handler actions. after `set_text`, thing
keeps the supplied bytes available to other applications until the selection is
replaced, the compositor cancels it, or thing exits. text must be valid utf-8,
contain no NUL byte, and be no larger than 1 mib. reads and writes time out after
two seconds, and retained source text is bounded to 4 mib.

the service prefers `ext-data-control-v1` and falls back to
`wlr-data-control-v1`. if the compositor supports neither protocol,
`available:get()` remains false and `error:get()` explains why. there is no
focus-dependent `wl_data_device` fallback for this desktop-wide service.

thing does not keep clipboard history and does not expose the primary selection,
images, or arbitrary mime types. without `--clipboard`, the service is
unavailable with no error and performs no clipboard work. `thing check` never
connects to wayland.

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

buttons, sliders, ranges, and text inputs accept `autofocus = true`. one widget
per panel or popup may use it. thing applies autofocus when that surface first
receives keyboard focus; an earlier pointer or explicit focus choice wins.

event handlers can move focus or reveal a row after updating state:

```luau
local search = ui.text_input {
    autofocus = true,
    value = '',
    on_change = function() end,
}
local result = ui.button { key = 'first-result' }

ui.button {
    on_click = function()
        ui.focus(search)
        ui.reveal(result)
    end,
}
```

`ui.focus(widget)` accepts an interactive widget. it also reveals that widget
when its panel or popup has keyboard focus; otherwise the request waits for that
surface's next keyboard entry. a popup created by the same event can claim the
request. if that popup is still closed when the next frame is prepared, the
request expires instead of carrying into a later opening. `ui.reveal(widget)`
moves every enclosing vertical scroll view by the smallest amount needed. both
functions are available only inside event handlers and take effect after the
handler's reactive update passes validation. hidden, disabled, and zero-sized
targets are ignored. if an updated list recreates the target, give it a stable
`key`. a container recreated around that target needs its own key as well. an
unkeyed positional replacement does not inherit focus or a queued action. hiding,
disabling, or removing the focused widget clears focus and text input state. a
successful configuration reload starts a new focus lifetime and can apply its
autofocus choice, while a failed reload leaves the current one alone.

panels and popups may handle keys while their surface has keyboard focus. the
callback runs before the built-in text, control, tab, and escape behavior:

```luau
local result = ui.button { key = 'first-result' }

return ui.panel {
    on_key = function(event)
        if event.key == 'Down' then
            ui.focus(result)
            ui.reveal(result)
            return true
        end
        return false
    end,
    children = { result },
}
```

`on_key(event)` must return a boolean. `true` consumes the press; `false` runs
the built-in behavior against the successfully refreshed tree. an error stops
that fallback. when the keymap marks a key repeatable, repetition continues
while either the callback consumes it or the built-in behavior repeats it. the
callback runs first for each repeat with `repeated = true`; key releases are not
exposed.

the event is a read-only record. `key` is the case-sensitive XKB keysym name,
such as `Down`, `Return`, `Escape`, or `a`; an unnamed symbol uses an eight-digit
hexadecimal value such as `0x01000000`. `text` is the UTF-8 text produced by the
physical press, or `nil` when it has no text, so shortcut matching should use
`key`. repeats retain that press's `key` and `text`. `shift`, `ctrl`, `alt`, and
`logo` report the effective modifier state when the callback runs; `logo` is
commonly called super or the windows key.

the host supports one grabbed popup at a time. an input event is required to open
it. outside clicks and escape dismiss it and call `on_close`. tab/shift+tab move
focus; return/space activate a focused button. slider and range arrows adjust by
`step` (or 1% of the range when step is zero); home/end reach the endpoints.
thing uses the compositor's rate and delay for its own repeat clock. if the
compositor takes over repetition, thing honors those repeated key events instead.
held slider and range arrows repeat. tab, escape, return/space on buttons, and
slider or range home/end act once per press.
