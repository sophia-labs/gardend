fn main() {
    // Gardend has no Tauri dependency and therefore no capability or bundle
    // metadata to generate. Keep Tauri's build helper in the desktop feature
    // closure only.
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
