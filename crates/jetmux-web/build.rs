//! Embeds the built web UI (`web/dist`, from `jetmux_web_build`) as a table
//! of files. Without a build, the server says how to make one.

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dist = Path::new(&std::env::var("CARGO_MANIFEST_DIR")?).join("web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut paths = Vec::new();
    if dist.is_dir() {
        files(&dist, &mut paths)?;
    }
    paths.sort();
    let mut table = String::from("static ASSETS: &[(&str, &[u8])] = &[\n");
    for path in &paths {
        let name = path
            .strip_prefix(&dist)?
            .to_string_lossy()
            .replace('\\', "/");
        writeln!(
            table,
            "    ({name:?}, include_bytes!({:?})),",
            path.display().to_string()
        )?;
    }
    if paths.is_empty() {
        table.push_str(
            "    (\"index.html\", b\"<!doctype html><title>jetmux</title><p>jetmux-web was built without its UI. Run <code>jetmux_web_build</code>, then rebuild.</p>\"),\n",
        );
    }
    table.push_str("];\n");
    std::fs::write(
        Path::new(&std::env::var("OUT_DIR")?).join("assets.rs"),
        table,
    )?;
    Ok(())
}
