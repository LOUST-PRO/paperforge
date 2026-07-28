# Contributing to lzt-wallcraft

Thanks for your interest. Quick orientation:

## Workspace layout

```
crates/lzt-wallcraft-core/    # lib, MIT
crates/lzt-wallcraft-cli/     # binary, depends on core
crates/lzt-wallcraft-tui/     # placeholder
```

All public APIs live in `lzt-wallcraft-core`. Prefer extending the
trait surface (`WallpaperBackend`) over adding new top-level
functions — it's how additional backends (swww, hyprpaper, mpvpaper)
will plug in.

## Adding a new backend

1. Add a variant to `BackendKind` in `crates/lzt-wallcraft-core/src/backend.rs`.
2. Implement `WallpaperBackend` for your backend struct.
3. Add a constructor (`MyBackend::new()`, optionally `with_binary()`).
4. Add unit tests covering the trait method happy paths.
5. Wire up the CLI subcommand in `crates/lzt-wallcraft-cli/src/main.rs`
   (or extend an existing subcommand).
6. Update `README.md` with the new backend row in the comparison table.

## Adding a new subcommand to the CLI

1. Add a variant to `Cmd` in `crates/lzt-wallcraft-cli/src/main.rs`.
2. Implement the handler in `match cli.cmd`.
3. Add an integration test in `crates/lzt-wallcraft-cli/tests/cli.rs`.

## Tests

- `cargo test --all` — must pass before any PR.
- `cargo clippy --all-targets --all-features -- -D warnings` — must
  pass before any PR.
- `cargo fmt --all -- --check` — must pass before any PR.

## License

By contributing, you agree that your contributions will be licensed
under the MIT License (see [`LICENSE`](LICENSE)). Your contribution
must be your own original work.

## Code style

- Sparse comments in source code (rationale + non-obvious gotchas).
  Discourse lives in PR descriptions and the module-level docstrings.
- English in code, comments, and PR bodies.
- No unsafe code (`#![forbid(unsafe_code)]` is set at the crate root).
- Clippy and rustfmt are the source of truth — run them.

## Commit messages

```
<type>(<scope>): <description>

<optional body explaining rationale>

<optional footer>
```

Types: `feat`, `fix`, `test`, `docs`, `refactor`, `chore`,
`perf`, `style`.

## Pull request flow

1. Branch from `master` (e.g. `feat/swww-backend`).
2. Keep diffs reviewable — < 1500 LOC when possible.
3. Sanitize before any push (no secrets, no internal IPs, no
   absolute paths in commits).
4. Open PR. CI must pass.
5. Reviewer (operator) approves or requests changes.
