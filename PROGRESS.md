# DictFlow — Mac + Windows progress (temporary)

DictFlow is offline dictation for **Windows and macOS**. Windows stays
GetAsyncKeyState + enigo Ctrl+V. macOS follows SpeakType: session event tap
+ Accessibility, CGEvent Cmd+V.

## Shipped in this branch

- [x] Platform-gated Cargo deps (no enigo on Mac, no objc2 on Windows)
- [x] Fn talk key via `kCGSessionEventTap` on the main run loop
- [x] Dev launches wrap as `DictFlow.app` so Accessibility lists DictFlow
- [x] Setup asks only for Microphone + Accessibility
- [x] Transparent logo / Dock / tray icons (no black plate)
- [x] Flow-style floating overlay: logical positioning, Dock clearance, always-on-top
- [x] Dead Mac/Windows code gated or removed (`hwnd_of`, restore split, keyring delete)

## Verify

- [x] cargo test / clippy / tsc
- [ ] Open the built `.dmg`, grant Accessibility, hold Fn, confirm overlay + paste
