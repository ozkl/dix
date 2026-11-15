//! On Windows, the executable gets the app's icon and version information
//! (what Explorer shows: the icon, and "Disk Image Explorer 0.1.0" under
//! Properties → Details). Needs the Windows SDK's rc.exe (MSVC) or windres
//! (MinGW, also for cross builds); without one the build goes on without
//! them, with a warning.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "Disk Image Explorer")
        .set("FileDescription", "Disk Image Explorer")
        .set("InternalName", "disk-image-explorer")
        .set("OriginalFilename", "disk-image-explorer.exe")
        .set("LegalCopyright", "Disk Image Explorer contributors (MIT license)");
    if let Err(e) = res.compile() {
        println!("cargo:warning=the Windows icon and version information were left out: {e}");
    }
}
