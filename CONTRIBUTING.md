# Contributing to DictFlow

Thanks for stopping by — DictFlow is MIT open source and PRs are welcome.

## Setup

Follow [docs/BUILD.md](docs/BUILD.md), then `npm install` and
`npm run tauri dev`.

## Before pushing

```powershell
npm run build
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

CI: [`.github/workflows/ci.yml`](.github/workflows/ci.yml). Frontend runs on
push. Rust check / clippy / test: Actions → ci → Run workflow.

## Ground rules

- **Offline-first.** No network calls except explicit model downloads and the
  (opt-in, repo-configured) update check. New features must work on a plane.
- **No new hard dependencies without discussion.** The hotkey poller is raw
  FFI on purpose; check `docs/ARCHITECTURE.md` constraints first (thread
  ownership of `!Send` audio/engine objects, static CRT, text-only clipboard).
- **Settings compatibility.** `settings.json` / `history.json` must load older
  files: `#[serde(default)]` new fields, migrate-then-save old shapes.
- **Every user-facing string needs a failure path.** Commands return
  `Result<_, String>`; the UI toasts errors — never fail silently.
- Keep functions small, comments explain *why*, tests cover pure logic in
  `text.rs` / `models.rs`.
