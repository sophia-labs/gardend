use crate::{
    profile_paths::profile_dir,
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{App, AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const MAIN_WINDOW_LABEL: &str = "main";
const WINDOW_TITLE: &str = "Garden";
const WINDOW_WIDTH: f64 = 1180.0;
const WINDOW_HEIGHT: f64 = 760.0;

/// How the native window's title bar is presented.
///
/// - `Full`: standard native title bar.
/// - `Minimal`: macOS "overlay" title bar - the window controls
///   (close/minimize/maximize) stay and float over the content, which extends
///   to the top edge, but the title bar chrome and title are hidden. The strip
///   stays draggable. On non-macOS this falls back to `Full`.
/// - `None`: fully frameless - no title bar and no window controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TitleBarMode {
    Full,
    Minimal,
    None,
}

impl Default for TitleBarMode {
    fn default() -> Self {
        Self::Full
    }
}

impl TitleBarMode {
    /// Whether the OS draws a frame (and, on macOS, the traffic-light controls).
    /// `Minimal` keeps decorations on - the controls live in the overlay bar.
    fn decorations(self) -> bool {
        !matches!(self, TitleBarMode::None)
    }

    /// Whether this mode uses the macOS transparent/overlay title-bar style.
    #[cfg(target_os = "macos")]
    fn overlay_title_bar(self) -> bool {
        matches!(self, TitleBarMode::Minimal)
    }
}

/// Persisted appearance/window preferences for the native shell. Kept as a
/// struct so future appearance prefs (theme, remembered geometry, etc.) slot in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowSettings {
    #[serde(default)]
    pub(crate) title_bar: TitleBarMode,
}

/// Legacy on-disk shape from the original boolean toggle, migrated forward.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyWindowSettings {
    decorations: bool,
}

fn window_settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("window-settings.json"))
}

pub(crate) fn read_window_settings(app: &AppHandle) -> Result<WindowSettings, String> {
    let path = window_settings_path(app)?;
    if !path.is_file() {
        return Ok(WindowSettings::default());
    }
    if let Ok(settings) = read_json::<WindowSettings>(&path) {
        return Ok(settings);
    }
    // Migrate the original `{ "decorations": bool }` file: off -> None, on -> Full.
    if let Ok(legacy) = read_json::<LegacyWindowSettings>(&path) {
        return Ok(WindowSettings {
            title_bar: if legacy.decorations {
                TitleBarMode::Full
            } else {
                TitleBarMode::None
            },
        });
    }
    Ok(WindowSettings::default())
}

fn write_window_settings(app: &AppHandle, settings: &WindowSettings) -> Result<(), String> {
    let dir = profile_dir(app)?;
    create_dir_all(&dir)?;
    write_json(&window_settings_path(app)?, settings).map_err(Into::into)
}

/// Create the main window in code (rather than statically in tauri.conf.json)
/// so the macOS title-bar style can vary by the persisted preference. The
/// creation-time style avoids a first-frame flash; runtime switches go through
/// [`apply_title_bar_mode`].
pub(crate) fn build_main_window(app: &App, mode: TitleBarMode) -> Result<(), String> {
    let builder = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::default())
        .title(WINDOW_TITLE)
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(true)
        .decorations(mode.decorations());

    #[cfg(target_os = "macos")]
    let builder = {
        use tauri::TitleBarStyle;

        if mode.overlay_title_bar() {
            builder
                .title_bar_style(TitleBarStyle::Overlay)
                .hidden_title(true)
        } else {
            builder.title_bar_style(TitleBarStyle::Visible)
        }
    };

    builder
        .build()
        .map_err(|error| format!("build main window: {error}"))?;
    Ok(())
}

/// Apply a title-bar mode to a live window - no restart. Toggles the OS frame
/// and, on macOS, the transparent/overlay title-bar attributes.
pub(crate) fn apply_title_bar_mode(
    window: &WebviewWindow,
    mode: TitleBarMode,
) -> Result<(), String> {
    window
        .set_decorations(mode.decorations())
        .map_err(|error| format!("set decorations: {error}"))?;

    #[cfg(target_os = "macos")]
    if mode.decorations() {
        apply_macos_title_bar(window, mode.overlay_title_bar())?;
    }

    Ok(())
}

/// Flip the macOS title-bar between standard and overlay at runtime. Must touch
/// AppKit on the main thread, so the work is dispatched there.
#[cfg(target_os = "macos")]
fn apply_macos_title_bar(window: &WebviewWindow, overlay: bool) -> Result<(), String> {
    use objc2::runtime::AnyObject;

    // NSWindowStyleMaskFullSizeContentView = 1 << 15
    const FULL_SIZE_CONTENT_VIEW: u64 = 1 << 15;
    // NSWindowTitleVisibility: Visible = 0, Hidden = 1
    let title_visibility: i64 = if overlay { 1 } else { 0 };

    let win = window.clone();
    window
        .run_on_main_thread(move || {
            let Ok(ns_window) = win.ns_window() else {
                return;
            };
            let ns_window = ns_window as *mut AnyObject;
            // SAFETY: ns_window() hands back the live NSWindow* and this runs on
            // the main thread, satisfying AppKit's threading requirement.
            unsafe {
                let _: () = objc2::msg_send![ns_window, setTitlebarAppearsTransparent: overlay];
                let _: () = objc2::msg_send![ns_window, setTitleVisibility: title_visibility];
                let mask: u64 = objc2::msg_send![ns_window, styleMask];
                let new_mask = if overlay {
                    mask | FULL_SIZE_CONTENT_VIEW
                } else {
                    mask & !FULL_SIZE_CONTENT_VIEW
                };
                let _: () = objc2::msg_send![ns_window, setStyleMask: new_mask];
            }
        })
        .map_err(|error| format!("dispatch title-bar update: {error}"))?;

    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_window_appearance(app: AppHandle) -> Result<WindowSettings, String> {
    read_window_settings(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_window_appearance(app: AppHandle, mode: TitleBarMode) -> Result<(), String> {
    write_window_settings(&app, &WindowSettings { title_bar: mode })?;
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main webview window not found".to_string())?;
    apply_title_bar_mode(&window, mode)
}
