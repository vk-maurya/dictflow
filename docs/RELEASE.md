# Release process

## Versioning

One version, three places — keep them in sync:

1. `package.json` → `version`
2. `src-tauri/Cargo.toml` → `[package] version`
3. `src-tauri/tauri.conf.json` → `version`

The app displays `env!("CARGO_PKG_VERSION")` (Settings → About), so the Rust
version is the source of truth at runtime. Use SemVer (`0.2.0`, `0.3.0`…).

## What to do for a release

1. Bump the three versions above.
2. Update the model/engine notes in README if anything changed.
3. Regenerate icons only if `assets/logo-512.png` changed:
   `npx tauri icon assets/logo-512.png`.
4. Full local check: `npm run build`, `cargo clippy … -- -D warnings`,
   `cargo test`, then `npm run tauri build`.
5. Find artifacts under `src-tauri/target/release/bundle/`:
   - `nsis/DictFlow_<ver>_x64-setup.exe` — recommended for users
   - `msi/DictFlow_<ver>_x64_en-US.msi` — enterprise deployment
   - `portable/DictFlow.exe` — double-click, no installer (from `build-windows.ps1`)
6. Smoke-test the NSIS installer on a clean machine/VM (mic + hotkey + one
   dictation + model download).
7. Push a tag — CI builds and attaches both installers to a **draft** release:
   `git tag v0.2.0; git push origin v0.2.0`, then review + publish at
   GitHub → Releases.

See [`.github/workflows/release.yml`](../.github/workflows/release.yml).

## In-app update check

`Settings → About → Check for updates` compares the running version against
the latest GitHub release. It is **disabled** until the repo exists: set
`UPDATE_CHECK_REPO` in `src-tauri/src/main.rs` to `"owner/repo"`.

## Code signing (recommended before wide distribution)

Unsigned installers trigger Windows SmartScreen warnings. Options:

- **Managed identity**: Azure Trusted Signing (pay-per-sign, no HSM to own).
- **OV/EV certificate**: install to the cert store; Tauri picks it up via
  env (`WINDOWS_CERTIFICATE_THUMBPRINT`, … — see `tauri-action` docs).
- Document the chosen thumbprint/secret setup in this file when you adopt one.

The updater (`tauri-plugin-updater` + signed bundles) is intentionally not
wired yet — the manual check above covers v0.2. Add it when signing exists.
