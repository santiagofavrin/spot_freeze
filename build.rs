//! Build script: embed `assets/icon.ico` as the exe's icon resource on
//! Windows. A no-op on every other target (and `winresource` stays unused
//! there), so linux/mac builds are unaffected.

fn main() {
    // `CARGO_CFG_TARGET_OS` describes the TARGET; `cfg!(windows)` in a build
    // script would describe the HOST and misfire on cross builds.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("failed to embed assets/icon.ico: {e}");
            std::process::exit(1);
        }
    }
}
