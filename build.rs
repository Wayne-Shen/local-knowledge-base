fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rerun-if-changed=src/folder_picker/macos.m");
    println!("cargo:rerun-if-changed=src/folder_picker/Info.plist");
    cc::Build::new()
        .file("src/folder_picker/macos.m")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .compile("kb_folder_picker");
    for framework in ["AppKit", "CoreGraphics", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    let info =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/folder_picker/Info.plist");
    // Give the executable an app identity and name even when it is run directly by launchd.
    for arg in [
        "-sectcreate",
        "__TEXT",
        "__info_plist",
        info.to_str().unwrap(),
    ] {
        println!("cargo:rustc-link-arg=-Xlinker");
        println!("cargo:rustc-link-arg={arg}");
    }
}
