# thing

thing is a framework for building bars, panels, and desktop widgets for wayland.
it's written in rust, with luau for defining your interface and its behavior.

the project is in early development, and the api is still changing.

## installation

to build thing, you'll need rust 1.88 or newer, a c/c++ compiler, and pkg-config.
you'll also need the development packages for these system libraries:

- wayland and egl, for displaying panels with the default opengl es renderer
- xkbcommon, for keyboard input
- fontconfig, for finding installed fonts
- pulseaudio, for reading and controlling audio devices

if you use pipewire, its pulseaudio-compatible server handles the audio
connection. you'll still need the pulseaudio development library to build thing.

your wayland compositor needs to support `wlr-layer-shell`.
this is the protocol that lets thing place a bar along the edge of your screen
and reserve space for it.

from the repository directory, install thing with cargo:

```sh
cargo install --locked --path crates/thing
```

vulkan support is available as an optional build feature. it uses the system's
vulkan driver through wgpu, while keeping opengl es as the default renderer:

```sh
cargo install --locked --path crates/thing --features vulkan
thing run --renderer vulkan path/to/panel.luau
```

renderer selection is explicit. if vulkan initialization fails, thing reports
the error instead of silently switching to opengl.

the [configuration guide](docs/configuration.md) walks through creating your
first panel, then covers widgets, state, styling, and services. if you'd like to
work on the framework, see [contributing](CONTRIBUTING.md).

## roadmap

- [x] luau configurations, reactive state, and reloads
- [x] panels, popups, layout, text, and images
- [x] clock, output audio controls, and niri workspaces and windows
- [x] per-output configuration
- [x] application launching
- [x] panel sizing and input regions
- [x] popup positioning controls
- [x] hover popups
- [x] scrolling containers
- [ ] text input and consistent key repeat
- [x] vulkan rendering alongside opengl
- [ ] drag and drop, animations, and custom shapes
- [x] microphone controls
- [x] per-application audio controls
- [ ] media, tray, clipboard, and notification services
- [ ] session locking and idle inhibition
- [ ] accessibility and complete keyboard navigation

## license

[mit](LICENSE-MIT) or [apache-2.0](LICENSE-APACHE).
