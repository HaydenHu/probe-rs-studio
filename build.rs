//! Embed the Windows executable icon.
//!
//! `assets/app-icon.res` is compiled once from `assets/app-icon.rc` with the
//! Windows SDK resource compiler (see the README). Keeping the prebuilt
//! resource in the repository means a release build needs neither the SDK
//! tools nor a build-dependency such as `winres`, and works fully offline.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo");
    let resource = std::path::Path::new(&manifest)
        .join("assets")
        .join("app-icon.res");

    println!("cargo:rerun-if-changed={}", resource.display());
    if resource.exists() {
        // link.exe accepts a compiled .res file directly as an input object.
        println!("cargo:rustc-link-arg={}", resource.display());
    } else {
        println!(
            "cargo:warning=Windows icon resource is missing: {}",
            resource.display()
        );
    }
}
