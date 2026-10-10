//! Embeds the Strata icon and version information in `strata-helper.exe`.
//!
//! NOTE: the helper is the program Windows names in the UAC prompt when Strata
//! asks for administrator rights, so it needs its own icon and description;
//! without them the prompt shows a generic icon and the bare file name.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=../../src-tauri/icons/icon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../src-tauri/icons/icon.ico")
        .set("FileDescription", "Strata helper")
        .set("ProductName", "Strata")
        .set("CompanyName", "Strata contributors")
        .set(
            "LegalCopyright",
            "Copyright (c) Strata contributors. MIT License.",
        )
        .set("OriginalFilename", "strata-helper.exe");
    res.compile()
        .expect("embed the helper's icon and version resources");
}
