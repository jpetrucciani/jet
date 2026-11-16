//! Embeds the app icon in Windows builds: `assets/jet.rc` is compiled with
//! `llvm-rc` (from the dev shell; `RC` overrides it) and the `.res` handed
//! straight to the linker.

use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets/jet.rc");
    println!("cargo:rerun-if-changed=assets/jet.ico");
    println!("cargo:rerun-if-env-changed=RC");
    if std::env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    let res = PathBuf::from(std::env::var("OUT_DIR")?).join("jet.res");
    let rc = std::env::var("RC").unwrap_or_else(|_| "llvm-rc".to_owned());
    let status = Command::new(&rc)
        .current_dir("assets")
        .arg("/fo")
        .arg(&res)
        .arg("jet.rc")
        .status()
        .map_err(|err| format!("running {rc} (in the dev shell; RC overrides): {err}"))?;
    if !status.success() {
        return Err(format!("{rc} failed: {status}").into());
    }
    println!("cargo:rustc-link-arg-bins={}", res.display());
    Ok(())
}
