# thing

thing is a framework for building bars, panels, and desktop widgets for wayland.
it's written in rust, with luau for defining your interface and its behavior.

the project is in early development, and the api is still changing.

## installation

to build thing, you'll need rust 1.88 or newer, a c/c++ compiler, libclang,
curl, and pkg-config. you'll also need the development packages for these
system libraries:

- wayland, egl, and opengl es, for displaying panels with the default renderer
- xkbcommon, for keyboard input
- fontconfig and freetype, for finding and drawing installed fonts
- pulseaudio, for reading and controlling audio devices
- pam, for the session-lock authentication helper

if you use pipewire, its pulseaudio-compatible server handles the audio
connection. you'll still need the pulseaudio development library to build thing.

your wayland compositor needs to support `wlr-layer-shell`.
this is the protocol that lets thing place a bar along the edge of your screen
and reserve space for it.

from the repository directory, install thing with cargo:

```sh
cargo install --locked --path crates/thing
```

the first build downloads an official prebuilt skia archive when one is
available for your platform and selected features. if there isn't a matching
archive, rust-skia builds skia locally; that fallback also needs clang, python
3, git, and ninja, and takes considerably longer. offline builds can point
`SKIA_BINARIES_URL` at a previously downloaded archive using a `file://` url.

vulkan support is available as an optional build feature. it uses skia with
your system's vulkan driver, while keeping opengl es as the default renderer:

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
- [x] multiple independent surfaces on each output
- [x] clock, power state, output audio controls, and niri workspaces and windows
- [x] per-output configuration
- [x] application launching
- [x] managed child processes with output and exit state
- [x] panel sizing and input regions
- [x] popup positioning controls
- [x] hover popups
- [x] scrolling containers
- [x] consistent keyboard repeat
- [x] text input with wayland input methods
- [x] text input clipboard shortcuts
- [x] vulkan rendering alongside opengl
- [x] custom vector shapes
- [x] visual translation, scale, and rotation
- [x] spring animations
- [ ] drag and drop
- [x] microphone controls
- [x] per-application audio controls
- [x] media metadata and playback controls
- [x] desktop notifications
- [x] clipboard text service
- [x] system tray
- [x] session locking
- [x] surface idle inhibition
- [ ] idle monitoring and system sleep inhibition
- [ ] accessibility and complete keyboard navigation

## license

[mit](LICENSE-MIT) or [apache-2.0](LICENSE-APACHE).

the renderer includes [skia](LICENSE-SKIA) and
[rust-skia](LICENSE-RUST-SKIA) under their respective licenses. notices for
the renderer and other dependencies that require attribution are in
[third-party notices](THIRD_PARTY_NOTICES).
