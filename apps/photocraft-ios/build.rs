//! Compiles the ObjC scene delegate into the binary (iOS targets only). See
//! `ios/PCSceneDelegate.m` for why it exists; `cc` picks the SDK and arch from the target
//! triple, and UIKit/objc are already linked by the egui stack.

fn main() {
    println!("cargo:rerun-if-changed=ios/PCSceneDelegate.m");
    println!("cargo:rerun-if-changed=ios/PCDocumentPicker.m");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("-ios") {
        let out = std::env::var("OUT_DIR").unwrap_or_default();
        // -fobjc-arc: the delegate holds statics (the pending-window list, the active scene) and
        // swizzles initWithFrame:, whose swapped return must follow the init-family retain rule —
        // under manual retain/release the autoreleased static array dangles after the first pool
        // drain and the next fast-enumeration crashes (NSInvalidArgumentException on the reused
        // memory, PhotoCraft-2026-11-09-003123.ips).
        cc::Build::new()
            .file("ios/PCSceneDelegate.m")
            .file("ios/PCDocumentPicker.m")
            .flag("-fobjc-arc")
            .flag("-Wno-deprecated-declarations") // the open picker's document-types init is the pre-UTType API
            .compile("pcscenedelegate");
        // The Rust side references the picker's pc_* symbols, but nothing references the scene
        // delegate's ObjC class, so the Apple linker would drop the whole archive as
        // unreferenced; force it in — the class must exist at launch for the Info.plist scene
        // manifest to instantiate.
        println!("cargo:rustc-link-arg-bins=-Wl,-force_load,{out}/libpcscenedelegate.a");
    }
}
