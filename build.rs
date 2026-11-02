// Compiles res/streetpad.rc (icon, manifest, version info) with llvm-rc and links it.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=res");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") { return; }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let res = out.join("streetpad.res");
    let status = Command::new(env::var("RC").unwrap_or_else(|_| "llvm-rc".into()))
        .args(["/nologo", "/fo"]).arg(&res).arg("res/streetpad.rc")
        .status().expect("failed to run llvm-rc (install llvm)");
    assert!(status.success(), "llvm-rc failed");
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
