# Contributing to DictFlow

Thanks for stopping by — DictFlow is MIT open source and PRs are welcome.
It runs on **Windows and macOS** with identical features and UI on both platforms.

## Setup

Follow [docs/BUILD.md](docs/BUILD.md) for your platform, then `npm install` and
`npm run tauri dev`.

## Before pushing

```bash
npm run build
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

CI: [`.github/workflows/ci.yml`](.github/workflows/ci.yml). Frontend runs on
push. Rust check / clippy / test: Actions → ci → Run workflow (runs on both
`windows-latest` and `macos-latest`).

## Ground rules

- **Offline-first.** No network calls except explicit model downloads and the
  (opt-in, repo-configured) update check. New features must work on a plane.
- **Cross-platform from day one.** Any platform-specific code must be gated
  with `#[cfg(target_os = "...")]`. Windows and macOS must both compile and
  behave correctly. Test on the CI matrix before merging.
- **No new hard dependencies without discussion.** Platform-specific APIs
  (hotkey polling, focus tracking, credential storage) already use gated
  modules; check `docs/ARCHITECTURE.md` constraints first (thread ownership of
  `!Send` audio/engine objects, static CRT on Windows, text-only clipboard).
- **Settings compatibility.** `settings.json` / `history.json` must load older
  files: `#[serde(default)]` new fields, migrate-then-save old shapes.
- **Every user-facing string needs a failure path.** Commands return
  `Result<_, String>`; the UI toasts errors — never fail silently.
- Keep functions small, comments explain *why*, tests cover pure logic in
  `text.rs` / `models.rs`.
