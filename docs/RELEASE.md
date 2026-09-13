# Release process

## Versioning

One version, three places — keep them in sync:

1. `package.json` → `version`
2. `src-tauri/Cargo.toml` → `[package] version`
3. `src-tauri/tauri.conf.json` → `version`

The app displays `env!("CARGO_PKG_VERSION")` (Settings → About). Use SemVer
(`0.2.0`, `0.3.0`…).

## What users download

| File | What it is |
|------|------------|
| `DictFlow_<ver>_x64-setup.exe` | NSIS installer. Recommended. Current-user install, Start Menu, uninstall. |
| `DictFlow-<ver>-windows-x64-portable.exe` | Same binary, no installer. Double-click to run. |
| `DictFlow_<ver>_x64_en-US.msi` | Optional enterprise/IT install. |

The portable build is **not** a USB sidecar: settings, models, and history
still live in `%APPDATA%\com.dictflow.app\dictflow`.
Both artifacts are unsigned, so SmartScreen may warn.

## How to publish (recommended)

1. Bump the three versions above.
2. Commit on the branch you want to ship.
3. Tag and push:

   ```powershell
   git tag v0.2.0
   git push origin v0.2.0
   ```

4. GitHub Actions (`.github/workflows/release.yml`) builds on Windows and
   opens a **draft** release with the installer, MSI, and portable exe.
5. Open GitHub → Releases, smoke-check the draft, then **Publish**.

`Settings → About → Check for updates` reads the latest *published* release
from `vk-maurya/dictflow`. Drafts do not count.

## Local build (optional, no tag)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
```

Artifacts land under `src-tauri/target/release/bundle/`:

- `nsis/DictFlow_<ver>_x64-setup.exe`
- `msi/DictFlow_<ver>_x64_en-US.msi`
- `portable/DictFlow.exe` (easy local double-click)
- `portable/DictFlow-<ver>-windows-x64-portable.exe` (same file, release name)

You can attach those to a GitHub Release by hand if CI is not used.

## Code signing (later)

Unsigned installers trigger Windows SmartScreen. When you are ready to
distribute widely:

- Azure Trusted Signing, or an OV/EV certificate via
  `WINDOWS_CERTIFICATE_THUMBPRINT` (see `tauri-action` docs).

The silent in-app updater (`tauri-plugin-updater`) is not wired yet. The
manual check in Settings is enough for v0.2.
