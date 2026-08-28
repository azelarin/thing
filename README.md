# thing

thing is a framework for building bars, panels, and desktop widgets for wayland.
it's written in rust, with luau for defining your interface and its behavior.

the project is in early development, and the api is still changing.

## installation

to build thing, you'll need rust 1.88 or newer, a c/c++ compiler, and pkg-config.
you'll also need the development packages for these system libraries:

- wayland and egl, for displaying panels and setting up graphics rendering
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

the [configuration guide](docs/configuration.md) walks through creating your
first panel, then covers widgets, state, styling, and services. if you'd like to
work on the framework, see [contributing](CONTRIBUTING.md).

## roadmap

- [x] luau configurations, reactive state, and reloads
- [x] panels, popups, layout, text, and images
- [x] clock, output audio controls, and niri workspaces and windows
- [ ] per-output configuration, sizing, and input regions
- [ ] hover popups, scrolling containers, text input, and keyboard repeat
- [ ] drag and drop, animations, and custom shapes
- [ ] microphone and per-application audio controls
- [ ] media, tray, clipboard, and notification services
- [ ] session locking and idle inhibition
- [ ] accessibility and complete keyboard navigation

## license

[mit](LICENSE-MIT) or [apache-2.0](LICENSE-APACHE).
