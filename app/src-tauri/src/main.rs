// trimwire Flightdeck — desktop shell (POC scaffold).
//
// Deliberately tiny: the window (configured in tauri.conf.json) points at the
// daemon's loopback cockpit (http://127.0.0.1:8766), so this shell reuses the
// exact web frontend the trimwire binary serves. The daemon is a *separate*
// process reached over the loopback control API — there is no sidecar here, so
// nothing to notarize beyond the app itself. See ../README.md and
// ../../docs/cockpit/05-multiplatform-app.md.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running trimwire Flightdeck");
}
