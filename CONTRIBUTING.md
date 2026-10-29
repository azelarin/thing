# contributing

keep each pull request focused on one change. explain what it does and how you
checked it. if it changes the luau api, update the configuration guide and
`crates/thing/types/thing.d.luau` too.

## issues

if something breaks, open an issue with the steps to reproduce it, what you
expected, and what happened. a small luau configuration and the error output
usually help more than a description alone. for display problems, include your
compositor, graphics driver, and output scale.

feature requests are welcome too. describe what you're trying to build and where
the current api gets in your way.

assign yourself to an issue when you start working on it. include `closes #123`
in the fix's pull request description so the issue closes when it merges.

## pull requests

work on a branch and open a draft while the change is in progress. use
[conventional commits](https://www.conventionalcommits.org/), such as
`fix: preserve the panel after a failed reload`.

describe the problem and the resulting behavior in a few sentences. summarize
routine checks; include exact commands when a check needs reproducing. the draft
badge is enough to show that work is unfinished.

once the code has been reviewed and the checks pass, mark the pull request ready.
squash-merge it and delete the branch.

## checks

install the dependencies listed in the [readme](README.md), then run:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --no-deps
python3 -m unittest discover -s tools -p 'test_*.py'
```

when the luau api changes, update the definition fixtures as well. run them with
luau-lsp 1.70.1:

```sh
python3 tools/check_luau_types.py --analyzer /path/to/luau-lsp
```

text tests need an installed font, and socket tests need permission to bind local
unix sockets. for rendering or input changes, also try the affected behavior in
a wayland session.

the renderer fixtures are opt-in because they open a nested compositor and use
the gpu. after building the backend you want to check, run:

```sh
python3 tools/check_rendering.py --binary ./target/debug/thing --renderer vulkan image-fit stencil gradients shadows
```

the tool requires a wayland session, `dbus-run-session`, niri, and grim. it does
not install or build anything.

keep `Cargo.lock` when building. before updating dependencies, check that the
runtime tests pass: `mlua` 0.12.2 currently needs the locked `luau0-src` version.

a regression test should exercise the behavior that broke. use small test
configurations so the test is easy to understand on its own.

## writing

use lowercase prose in documentation, comments, and pull requests. keep the
required casing of code, commands, paths, license text, and `SAFETY:` markers.

write complete sentences and explain unfamiliar requirements. document behavior
that someone needs to use the api, and comment on decisions that the code alone
doesn't explain. skip descriptions of obvious steps and accounts of how the code
was developed.

contributions use the repository's mit or apache-2.0 license.
