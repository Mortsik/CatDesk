# Contributing

Thanks for contributing!

Write issues and PRs in whatever language you're comfortable with. It's 2026. We have LLMs. We'll manage.

## Development

1. Fork and clone the repository.
2. Create a new branch with `git switch -c feat/your-feature`.
3. Make your changes.
4. Run `cargo fmt`.
5. Run `cargo fmt --check` (must pass with zero diffs).
6. Run `cargo test --release`.
7. Run `cargo build --release`.
8. Open a pull request.

## Code Style

Formatting is rustfmt defaults, pinned for determinism:

- `rust-toolchain.toml` pins the exact toolchain (currently `1.93.0`), so plain
  `cargo fmt` / `cargo fmt --check` resolve to the same rustfmt for everyone
  (rustup installs the pinned toolchain on first use). Do not format with a
  different rustfmt — output differs between versions and styles.
- `rustfmt.toml` intentionally contains no settings; don't add any as part of
  another change. If a setting must change, run the resulting `cargo fmt` as a
  dedicated `style:` commit that contains nothing else.

Never mix reformatting into functional commits — keep `cargo fmt` output in its
own commit so diffs stay reviewable.

## Commits

Use clear commit messages. We recommend Conventional Commits:

- `feat:` new feature
- `fix:` bug fix
- `docs:` documentation
- `refactor:` refactoring
- `test:` tests

## Pull Requests

Open pull requests directly to `main`.

## Issues

If you'd like to open an issue, feel free to do so.
