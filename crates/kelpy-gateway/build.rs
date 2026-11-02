//! Embeds the built Web UI (`web/dist`) into the executable.
//!
//! Generates `$OUT_DIR/assets.rs` with one `include_bytes!` per file. When the Web UI has not been built, the list
//! is empty and the server answers with a page explaining how to build it, so `cargo build` always works.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <workspace>/crates/kelpy-gateway");
    let dist = workspace_dir.join("web").join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
        files.sort();
    } else {
        println!(
            "cargo:warning=The Web UI is not built, so it is not embedded. Run `bun install` and `bun run build` in web/."
        );
    }

    let mut code = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (relative, absolute) in &files {
        code.push_str(&format!(
            "    ({relative:?}, include_bytes!({:?})),\n",
            absolute.display().to_string()
        ));
    }
    code.push_str("];\n");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    fs::write(out_dir.join("assets.rs"), code).expect("cannot write assets.rs");
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("cannot read web/dist") {
        let path = entry.expect("cannot read web/dist").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("file is under web/dist")
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
}
