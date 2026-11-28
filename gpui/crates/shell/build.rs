fn main() {
    // Both desktops identify as the Iced package version on the ACP wire.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let desktop = std::fs::read_to_string(manifest).expect("read desktop manifest");
    let version = desktop
        .lines()
        .skip_while(|line| *line != "[package]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| line.trim().strip_prefix("version = \"")?.strip_suffix('"'))
        .expect("desktop package version");
    println!("cargo:rustc-env=GITTERM_DESKTOP_VERSION={version}");
}
