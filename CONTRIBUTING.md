# contributing

keep changes focused. describe the resulting behavior and how you checked it.
use [conventional commits](https://www.conventionalcommits.org/).

work on a branch and open a draft pull request while the change is in progress.
mark it ready after review and the relevant checks pass. squash-merge completed
pull requests and delete their branches.

## checks

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --no-deps
python3 -m unittest discover -s tools -p 'test_*.py'
```

native dependencies are listed in the [readme](README.md). text tests need an
installed font; ci uses dejavu. socket tests need permission to bind local unix
sockets. use the [desktop checklist](#desktop-testing) for changes to rendering,
input, or platform integration.

keep `Cargo.lock` when building. with `mlua` 0.12.2, `luau0-src` is pinned to
0.22.0+luau740 because 0.22.1+luau744 failed during vm initialization. runtime
tests must pass before changing that dependency.

## code

the runtime turns a luau configuration into a validated widget tree. layout and
rendering use that tree without calling into luau. if you're looking for where a
change belongs, start here:

| crate | responsibility |
| --- | --- |
| `thing-core` | widget values, geometry, colors, validation |
| `thing-runtime` | luau execution, reactive state, event handlers, service api |
| `thing-ui` | layout, text shaping, painting, image loading, hit testing |
| `thing-wayland` | surfaces, input, event loop, egl, file watching, service workers |
| `thing` | command-line interface |

taffy handles layout, parley shapes text, and femtovg draws through opengl es 2.
graphics objects stay on the event-loop thread. image loading, audio, and niri
communication run on workers that wake the host when results arrive.

keep platform objects outside the luau runtime and blocking work off the ui
thread. report recoverable failures through `Result`, with enough context to
identify the cause. test behavior that could regress, especially reloads,
dependency tracking, and resource cleanup.

## desktop testing

record the commit, compositor version, graphics driver, output sizes, and scales
when reporting a graphical test.

### panel and input

use a configuration that exercises the behavior you're changing. you can start
with the panel in the [configuration guide](docs/configuration.md#your-first-panel)
and run it with `cargo run --locked -- run /path/to/panel.luau`.

- check the panel on each output and the space reserved for it.
- check text, buttons, and hit targets at 100%, 125%, 150%, and 200% scale.
- open the popup near output edges. dismiss it with escape and an outside click.
- test tab/shift+tab, return/space, slider arrows, and home/end. disabled controls
  must ignore input.
- adjust volume and mute, then verify both in another mixer. make changes in that
  mixer and check that the panel follows.
- connect and disconnect an audio device. select another output, then restore the
  original device and volume.
- disconnect an output with a popup open. check the remaining panels, reconnect,
  and repeat with different output scales.

### reload and recovery

- edit an imported theme using an editor that saves files atomically.
- introduce a syntax error. the current panel should keep working. fix the error
  and save again.
- reload with a popup open. it should close and reopen on the next click.
- in a disposable session, restart the audio server. the panel should reconnect
  without applying commands queued before disconnection.
- exit with ctrl+c and check that the panel closes.

### process sampling

```sh
python3 tools/measure_process.py --pid 12345 --seconds 60 > idle.json
```

replace `12345` with the process id. the sampler reports cpu use and proportional
set size (pss), including observed child processes. one core is 100% cpu.
unreadable memory is reported as `null`; short-lived children may be missed.
startup time, wakeups, gpu allocation, and frame timing need separate measurements.

## writing

use lowercase prose and sentence punctuation. preserve the casing of code,
commands, paths, and license text. keep the `SAFETY:` marker on unsafe-code
comments.

use complete sentences and address the reader directly where it helps. give
enough context to follow an instruction, especially when a requirement may be
unfamiliar.

write documentation when it answers a question a caller or maintainer would
have. public api docs should explain behavior and relevant errors. implementation
comments should explain a constraint, assumption, or reason for a choice. avoid
restating names, narrating obvious steps, or recording development history in
source files.

use these terms consistently:

| term | meaning |
| --- | --- |
| shell | the desktop interface built with thing |
| configuration | a luau entry file and its imported modules |
| widget | an individual ui element |
| component | a reusable function that builds widgets |
| panel | a desktop surface, such as a bar |
| popup | a surface opened relative to a control |
| service | desktop state and actions exposed to luau |
| adapter | the implementation connecting a service to a platform or protocol |

contributions use the repository's mit or apache-2.0 license.
