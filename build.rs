fn main() {
    // Reproducible source identity even when Docker cannot follow the host .git file.
    let mut hash = 0xcbf29ce484222325u64;
    for path in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "assets/fonts",
        "vendor/gpui-base",
        "vendor/gpui-component",
    ] {
        fingerprint(std::path::Path::new(path), &mut hash);
    }
    println!("cargo:rustc-env=OPENCODE_BUILD_FINGERPRINT={hash:016x}");
}

fn fingerprint(path: &std::path::Path, hash: &mut u64) {
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        let mut paths: Vec<_> = std::fs::read_dir(path)
            .expect("source directory")
            .map(|entry| entry.expect("source entry").path())
            .collect();
        paths.sort();
        for path in paths {
            fingerprint(&path, hash);
        }
    } else {
        for byte in path
            .to_string_lossy()
            .bytes()
            .chain(std::iter::once(0))
            .chain(std::fs::read(path).expect("fingerprint input"))
        {
            *hash = (*hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
}
