mod winfsp {
    include!("../../build/winfsp.rs");
    pub fn configure() {
        main();
    }
}

fn main() {
    winfsp::configure();
    let target = std::env::var("CARGO_CFG_TARGET_OS").expect("missing target OS");
    println!("cargo:rustc-check-cfg=cfg(agentdrive_native_mount)");
    if matches!(target.as_str(), "linux" | "macos" | "windows") {
        println!("cargo:rustc-cfg=agentdrive_native_mount");
    }
}
