//! Embeds the console's build output, so that `mmx serve` needs nothing beside the binary. Without a
//! build in `console/dist` it compiles an empty table, because `mmx` must build with plain cargo.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dist = manifest.join("../../console/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut found = Vec::new();
    files(&dist, &mut found);
    found.sort();

    let mut table = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for path in &found {
        println!("cargo:rerun-if-changed={}", path.display());
        let served = path.strip_prefix(&dist).expect("found under dist");
        let served = served.to_string_lossy().replace('\\', "/");
        writeln!(
            table,
            "    ({:?}, include_bytes!({:?})),",
            format!("/{served}"),
            path
        )
        .expect("writing to a string");
    }
    table.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").expect("set by cargo"));
    fs::write(out.join("console_assets.rs"), table).expect("writing the asset table");
}
