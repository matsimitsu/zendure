use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/scss");
    println!("cargo:rerun-if-changed=assets/vendor");

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out_dir = Path::new(&out_dir);

    let css = grass::from_path(
        Path::new("assets/scss/index.scss"),
        &grass::Options::default(),
    )
    .unwrap_or_else(|e| panic!("failed to compile assets/scss/index.scss: {e}"));

    std::fs::write(out_dir.join("dashboard.css"), css)
        .expect("failed to write compiled dashboard.css to OUT_DIR");

    copy_vendored_scripts(out_dir);
}

/// Vendored so the dashboard works on a LAN with no internet: htmx 2.0.4 and
/// htmx-ext-sse 2.2.4, pinned in `assets/vendor/README.md`.
fn copy_vendored_scripts(out_dir: &Path) {
    let vendor = Path::new("assets/vendor");
    let entries = std::fs::read_dir(vendor).expect("failed to read assets/vendor");

    for entry in entries {
        let path = entry.expect("failed to read assets/vendor entry").path();
        if path.extension().is_none_or(|ext| ext != "js") {
            continue;
        }
        let name = path.file_name().expect("a file path has a name");
        std::fs::copy(&path, out_dir.join(name))
            .unwrap_or_else(|e| panic!("failed to copy {} to OUT_DIR: {e}", path.display()));
    }
}
