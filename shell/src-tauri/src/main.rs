//! Sophia: a native window onto a Sophia place.
//!
//! Two places, chosen from the Place menu and remembered between launches:
//! - **Hosted**: garden.sophia-labs.com (or `SOPHIA_URL`), with its own sign-in and storage.
//! - **This Mac**: a `gardend` the app starts beside itself, with no network, served the Shrubbery UI bundled in the app.
//!
//! The window opens every other site in the default browser and saves downloads to ~/Downloads
//! (plans/2026-10-09-native-bundle.md).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod local;

use std::sync::Mutex;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

use local::LocalCell;

const HOSTED: &str = "https://garden.sophia-labs.com/";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Hosted,
    Local,
}

struct CurrentPlace(Mutex<Place>);

fn hosted_url() -> Url {
    std::env::var("SOPHIA_URL")
        .ok()
        .and_then(|u| Url::parse(&u).ok())
        .unwrap_or_else(|| Url::parse(HOSTED).expect("the hosted address parses"))
}

/// The app's own origin, where the bundled Shrubbery is served.
fn local_url() -> Url {
    Url::parse(if cfg!(windows) { "http://tauri.localhost/" } else { "tauri://localhost/" }).expect("the app origin parses")
}

fn home(place: Place) -> Url {
    match place {
        Place::Hosted => hosted_url(),
        Place::Local => local_url(),
    }
}

fn place_file(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join("place"))
}

fn remembered_place(app: &AppHandle) -> Place {
    match place_file(app).and_then(|p| std::fs::read_to_string(p).ok()).as_deref().map(str::trim) {
        Some("local") => Place::Local,
        _ => Place::Hosted,
    }
}

fn remember_place(app: &AppHandle, place: Place) {
    if let Some(path) = place_file(app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, if place == Place::Local { "local\n" } else { "hosted\n" });
    }
}

/// A URL that belongs inside the window: either place, and the in-page schemes. Everything else is another site.
fn stays_inside(url: &Url) -> bool {
    let hosted = hosted_url();
    match url.scheme() {
        "about" | "blob" | "data" | "tauri" => true,
        "http" | "https" => {
            url.host_str() == Some("tauri.localhost")
                || (url.host_str() == hosted.host_str() && url.port_or_known_default() == hosted.port_or_known_default())
        }
        _ => false,
    }
}

fn open_elsewhere(app: &AppHandle, url: &Url) {
    if matches!(url.scheme(), "http" | "https" | "mailto") {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
    }
}

fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}

fn title(place: Place) -> &'static str {
    match place {
        Place::Hosted => "Sophia",
        Place::Local => "Sophia · This Mac",
    }
}

fn go_to(app: &AppHandle, place: Place) {
    match place {
        Place::Local => {
            if let Err(e) = app.state::<LocalCell>().ensure_started(app) {
                eprintln!("sophia: {e}");
            }
        }
        // The local cell flushes and stops; its data stays in the profile for next time.
        Place::Hosted => app.state::<LocalCell>().stop(),
    }
    *app.state::<CurrentPlace>().0.lock().expect("place lock") = place;
    remember_place(app, place);
    if let Some(menu) = app.menu() {
        for (id, on) in [("place-hosted", place == Place::Hosted), ("place-local", place == Place::Local)] {
            if let Some(item) = menu.get(id).and_then(|i| i.as_check_menuitem().cloned()) {
                let _ = item.set_checked(on);
            }
        }
    }
    if let Some(window) = main_window(app) {
        let _ = window.set_title(title(place));
        let _ = window.navigate(home(place));
    }
}

fn menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let place = remembered_place(app);
    let menu = Menu::default(app)?;
    let host = hosted_url().host_str().unwrap_or("hosted").to_owned();
    let places = Submenu::with_items(
        app,
        "Place",
        true,
        &[
            &CheckMenuItem::with_id(app, "place-hosted", format!("Hosted ({host})"), true, place == Place::Hosted, None::<&str>)?,
            &CheckMenuItem::with_id(app, "place-local", "This Mac (local, offline)", true, place == Place::Local, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "local-folder", "Show Local Data in Finder", true, None::<&str>)?,
        ],
    )?;
    let go = Submenu::with_items(
        app,
        "Go",
        true,
        &[
            &MenuItem::with_id(app, "back", "Back", true, Some("CmdOrCtrl+["))?,
            &MenuItem::with_id(app, "forward", "Forward", true, Some("CmdOrCtrl+]"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "home", "Home", true, Some("CmdOrCtrl+Shift+H"))?,
            &MenuItem::with_id(app, "reload", "Reload", true, Some("CmdOrCtrl+R"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "in-browser", "Open This Page in Browser", true, Some("CmdOrCtrl+Shift+O"))?,
        ],
    )?;
    menu.append(&places)?;
    menu.append(&go)?;
    Ok(menu)
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(LocalCell::new())
        .invoke_handler(tauri::generate_handler![
            local::get_loopback_manifest,
            local::get_provider_keys,
            local::store_provider_key,
            local::delete_provider_key,
        ])
        .menu(menu)
        .on_menu_event(|app, event| {
            let window = main_window(app);
            match event.id().as_ref() {
                "place-hosted" => go_to(app, Place::Hosted),
                "place-local" => go_to(app, Place::Local),
                "local-folder" => {
                    if let Ok(dir) = local::profile_dir(app) {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = app.opener().open_path(dir.to_string_lossy(), None::<&str>);
                    }
                }
                "back" => {
                    let _ = window.map(|w| w.eval("history.back()"));
                }
                "forward" => {
                    let _ = window.map(|w| w.eval("history.forward()"));
                }
                "reload" => {
                    let _ = window.map(|w| w.eval("location.reload()"));
                }
                "home" => {
                    let place = *app.state::<CurrentPlace>().0.lock().expect("place lock");
                    let _ = window.map(|w| w.navigate(home(place)));
                }
                "in-browser" => {
                    if let Some(url) = window.and_then(|w| w.url().ok()) {
                        open_elsewhere(app, &url);
                    }
                }
                _ => {}
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let place = remembered_place(&handle);
            app.manage(CurrentPlace(Mutex::new(place)));
            let start = match place {
                Place::Hosted => WebviewUrl::External(hosted_url()),
                Place::Local => {
                    if let Err(e) = handle.state::<LocalCell>().ensure_started(&handle) {
                        eprintln!("sophia: {e}");
                    }
                    WebviewUrl::App("index.html".into())
                }
            };
            let (nav_app, pop_app) = (handle.clone(), handle.clone());
            WebviewWindowBuilder::new(app, "main", start)
                .title(title(place))
                .inner_size(1280.0, 860.0)
                .min_inner_size(420.0, 360.0)
                .devtools(true)
                // Another site never replaces the place: it opens in the default browser.
                .on_navigation(move |url| {
                    let inside = stays_inside(url);
                    if !inside {
                        open_elsewhere(&nav_app, url);
                    }
                    inside
                })
                // window.open and target=_blank: the place's own pages load here, other sites in the browser.
                .on_new_window(move |url, _features| {
                    if stays_inside(&url) {
                        if let Some(window) = main_window(&pop_app) {
                            let _ = window.navigate(url);
                        }
                    } else {
                        open_elsewhere(&pop_app, &url);
                    }
                    NewWindowResponse::Deny
                })
                .on_download(|webview, event| {
                    if let DownloadEvent::Requested { destination, .. } = event {
                        if let (Ok(dir), Some(name)) = (webview.path().download_dir(), destination.file_name()) {
                            *destination = unique(dir.join(name));
                        }
                    }
                    true
                })
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Sophia could not start");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            app.state::<LocalCell>().stop();
        }
    });
}

/// `report.pdf`, then `report 2.pdf`, `report 3.pdf`: never overwrite a file already in Downloads.
fn unique(path: std::path::PathBuf) -> std::path::PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("download").to_owned();
    let ext = path.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    (2..).map(|n| path.with_file_name(format!("{stem} {n}{ext}"))).find(|p| !p.exists()).expect("a free name")
}
