// Build script for Chiroot.
//
// It only validates that the required assets are present (so a missing asset
// fails the build with a readable message instead of a cryptic compiler error)
// and then hands over to `tauri-build`, which reads `tauri.conf.json`,
// generates the ACL / capabilities and embeds the Windows icon resource.

use std::path::Path;

fn main() {
    // `favicon.png` is the application icon, `assets/error-sign.png` is the
    // artwork of the built-in connection error screen (embedded in the binary).
    for asset in ["favicon.png", "assets/error-sign.png"] {
        if !Path::new(asset).exists() {
            panic!(
                "required asset `{asset}` was not found.\n\
                 Run the build from the project root (the folder that contains Cargo.toml)."
            );
        }
        println!("cargo:rerun-if-changed={asset}");
    }

    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=capabilities");
    println!("cargo:rerun-if-changed=icons");

    tauri_build::build()
}
