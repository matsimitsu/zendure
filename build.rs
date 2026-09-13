use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/scss");

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let css = grass::from_path(
        Path::new("assets/scss/index.scss"),
        &grass::Options::default(),
    )
    .unwrap_or_else(|e| panic!("failed to compile assets/scss/index.scss: {e}"));

    std::fs::write(Path::new(&out_dir).join("dashboard.css"), css)
        .expect("failed to write compiled dashboard.css to OUT_DIR");
}
