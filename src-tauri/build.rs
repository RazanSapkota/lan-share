fn main() {
    // `tauri_build` compiles icons/icon.ico into a Windows resource and links it
    // as `resource.lib`, but the only path it asks cargo to watch is
    // tauri.conf.json. Replace the icon and nothing here re-runs: cargo relinks
    // the exe against the resource built the last time the config happened to
    // change, and the binary keeps the icon you just replaced -- with no error,
    // and no clue beyond a timestamp buried in target/.
    println!("cargo:rerun-if-changed=icons");
    println!("cargo:rerun-if-changed=icons/icon.ico");

    tauri_build::build()
}
