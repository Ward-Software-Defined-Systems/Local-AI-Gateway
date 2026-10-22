//! Stages the built dashboard for `rust-embed`.
//!
//! `src/web/assets.rs` embeds `$OUT_DIR/ui`. This script fills that directory from `frontend/dist` when
//! the frontend has been built, or with a one-page placeholder otherwise, so the Rust side always compiles
//! and tests without Node. It emits `rerun-if-changed` for the `frontend/` directory (whose mtime changes
//! when `dist/` appears or disappears) and for every entry under `dist/`, so a frontend-only rebuild
//! re-embeds. rust-embed's own change tracking is unreliable for release embeds; an earlier in-house web console
//! shipped stale bundles before it learnt this.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Local AI Gateway</title>
<style>body{font-family:system-ui,sans-serif;background:#09090b;color:#d4d4d8;margin:0;padding:3rem}code{background:#18181b;padding:.1rem .3rem;border-radius:.2rem}</style>
</head><body><h1>Local AI Gateway</h1>
<p>The dashboard has not been built into this binary. Run <code>make frontend</code> and rebuild, or use <code>make release</code>.</p>
<p>The API is available under <code>/api/</code>; try <code>/api/status</code>.</p></body></html>
"#;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let frontend = manifest_dir.join("..").join("..").join("frontend");
    let dist = frontend.join("dist");
    let ui = out_dir.join("ui");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", frontend.display());

    let _ = fs::remove_dir_all(&ui);
    fs::create_dir_all(&ui).expect("create OUT_DIR/ui");

    if dist.join("index.html").is_file() {
        copy_tree(&dist, &ui).expect("copy frontend/dist into OUT_DIR/ui");
        watch_tree(&dist);
    } else {
        fs::write(ui.join("index.html"), PLACEHOLDER).expect("write placeholder index.html");
        println!(
            "cargo:warning=frontend/dist not found: embedding a placeholder dashboard (run `make frontend` to build the UI)"
        );
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// `rerun-if-changed` for every file and directory under `dir`, so added and removed files count too.
fn watch_tree(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            watch_tree(&path);
        }
    }
}
