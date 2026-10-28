# contributing

keep each pull request focused on one change. explain what it does and how you
checked it. if it changes the luau api, update the configuration guide too.

## issues

if something breaks, open an issue with the steps to reproduce it, what you
expected, and what happened. a small luau configuration and the error output
usually help more than a description alone. for display problems, include your
compositor, graphics driver, and output scale.

feature requests are welcome too. describe what you're trying to build and where
the current api gets in your way.

## pull requests

work on a branch and open a draft while the change is in progress. use
[conventional commits](https://www.conventionalcommits.org/), such as
`fix: preserve the panel after a failed reload`.

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

text tests need an installed font, and socket tests need permission to bind local
unix sockets. for rendering or input changes, also try the affected behavior in
a wayland session.

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
