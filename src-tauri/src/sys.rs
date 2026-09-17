//! OS Settings deep-links (microphone, Accessibility, Input Monitoring).
//!
//! Windows uses `ms-settings:` URIs. macOS tries the current Settings pane
//! id first, then the pre-Ventura `preference.security` id.
//!
//! On macOS, a binary launched from the terminal is not an application as far
//! as TCC is concerned — Accessibility lists Terminal / Cursor, not DictFlow.
//! [`relaunch_from_app_bundle_if_needed`] copies the debug exe into a real
//! `.app` and `exec`s it so System Settings can show **DictFlow**.

/// Open the OS microphone privacy settings page.
#[tauri::command]
pub(crate) fn open_mic_settings() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:privacy-microphone"])
            .status()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        open_privacy_pane("Privacy_Microphone")?;
    }
    Ok(())
}

/// Open macOS System Settings → Privacy & Security → Accessibility.
/// On other platforms this is a no-op (the command is never called from the frontend).
#[tauri::command]
pub(crate) fn open_accessibility_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        open_privacy_pane("Privacy_Accessibility")?;
    }
    Ok(())
}

/// Open macOS System Settings → Privacy & Security → Input Monitoring.
/// On other platforms this is a no-op (the command is never called from the frontend).
#[tauri::command]
pub(crate) fn open_input_monitoring_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        open_privacy_pane("Privacy_ListenEvent")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_privacy_pane(query: &str) -> Result<(), String> {
    let urls = [
        format!("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?{query}"),
        format!("x-apple.systempreferences:com.apple.preference.security?{query}"),
    ];
    for url in urls {
        if std::process::Command::new("open")
            .arg(&url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
    }
    Err(format!("could not open System Settings ({query})"))
}

/// If this process is a naked `dictflow` binary (cargo / `tauri dev`), wrap it
/// in `DictFlow.app` next to the exe and replace this process. No-op when we
/// are already inside a bundle (release `.app`, or after this relaunch).
#[cfg(target_os = "macos")]
pub(crate) fn relaunch_from_app_bundle_if_needed() {
    if std::env::var_os("DICTFLOW_SKIP_APP_BUNDLE").is_some() {
        return;
    }
    if std::env::var_os("DICTFLOW_IN_APP_BUNDLE").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if is_inside_app_bundle(&exe) {
        return;
    }
    let name = exe.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name != "dictflow" {
        return;
    }
    match install_dev_app_bundle(&exe) {
        Ok(bundled) => {
            eprintln!(
                "dictflow: launching as {} so Accessibility can list DictFlow (not Terminal)",
                bundled
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .unwrap_or(bundled.as_path())
                    .display()
            );
            let mut cmd = std::process::Command::new(&bundled);
            cmd.args(std::env::args().skip(1));
            cmd.env("DICTFLOW_IN_APP_BUNDLE", "1");
            cmd.env("__CFBundleIdentifier", "com.dictflow.app");
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            eprintln!("dictflow: exec of app bundle failed: {err}");
        }
        Err(e) => {
            eprintln!("dictflow: could not wrap as DictFlow.app ({e}); Accessibility will not list this process");
        }
    }
}

#[cfg(target_os = "macos")]
fn is_inside_app_bundle(exe: &std::path::Path) -> bool {
    exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

#[cfg(target_os = "macos")]
fn install_dev_app_bundle(exe: &std::path::Path) -> Result<std::path::PathBuf, String> {
    use std::fs;
    use std::path::PathBuf;

    let app = exe
        .parent()
        .ok_or("exe has no parent directory")?
        .join("DictFlow.app");
    let contents = app.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&resources).map_err(|e| e.to_string())?;

    let version = env!("CARGO_PKG_VERSION");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>DictFlow</string>
	<key>CFBundleExecutable</key>
	<string>dictflow</string>
	<key>CFBundleIdentifier</key>
	<string>com.dictflow.app</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>DictFlow</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>CFBundleIconFile</key>
	<string>icon.icns</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.productivity</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>DictFlow needs microphone access to capture your voice for offline dictation.</string>
</dict>
</plist>
"#
    );
    fs::write(contents.join("Info.plist"), plist).map_err(|e| e.to_string())?;
    fs::write(contents.join("PkgInfo"), b"APPL????").map_err(|e| e.to_string())?;

    let icon_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("icons/icon.icns");
    if icon_src.is_file() {
        let _ = fs::copy(&icon_src, resources.join("icon.icns"));
    }

    let bundled = macos_dir.join("dictflow");
    if bundled.exists() {
        let _ = fs::remove_file(&bundled);
    }
    fs::copy(exe, &bundled).map_err(|e| format!("copy into app bundle: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bundled).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bundled, perms).map_err(|e| e.to_string())?;
    }

    let _ = std::process::Command::new("codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            "com.dictflow.app",
        ])
        .arg(&bundled)
        .status();
    let _ = std::process::Command::new("codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            "com.dictflow.app",
        ])
        .arg(&app)
        .status();

    let lsregister = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = std::process::Command::new(lsregister)
        .args(["-f"])
        .arg(&app)
        .status();

    Ok(bundled)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn naked_debug_binary_is_not_a_bundle() {
        assert!(!is_inside_app_bundle(std::path::Path::new(
            "/tmp/target/debug/dictflow"
        )));
        assert!(is_inside_app_bundle(std::path::Path::new(
            "/tmp/target/debug/DictFlow.app/Contents/MacOS/dictflow"
        )));
    }
}
