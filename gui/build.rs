//! Build script for ampaper-gui.
//!
//! Sole job: copy the vendored PDFium binary into the cargo target
//! directory next to the compiled `ampaper-gui` executable so PDF
//! input on the Decode tab Just Works without the user having to
//! download anything separately. The DLL ships in-repo under
//! `vendor/pdfium/<target>/` — see vendor/pdfium/NOTICE.md for the
//! license + provenance story.
//!
//! Why a build script and not, say, a `cargo:rustc-link-arg`-based
//! static link: dynamic + bundled DLL keeps the binary lean and
//! the cross-platform story simple. The cost is one extra file
//! shipped alongside the .exe.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=vendor/pdfium");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // Map target → vendored subdirectory + binary filename. We
    // currently ship the Windows x64 binary; Linux and macOS
    // entries are stubbed out with helpful messages so a user
    // running `cargo build` on those platforms gets a clear
    // "drop the binary here" pointer instead of a silent no-op.
    let (subdir, filename) = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", "x86_64") => ("win64", "pdfium.dll"),
        ("linux", "x86_64") => ("linux-x64", "libpdfium.so"),
        ("macos", "x86_64") => ("mac-x64", "libpdfium.dylib"),
        ("macos", "aarch64") => ("mac-arm64", "libpdfium.dylib"),
        _ => {
            println!(
                "cargo:warning=ampaper-gui: no vendored PDFium for target {target_os}/{target_arch}; \
                 PDF decode will report \"PDFium library not found\" until you place the right \
                 platform binary alongside the executable. Pre-built binaries: \
                 https://github.com/bblanchon/pdfium-binaries/releases"
            );
            return;
        }
    };

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let src = PathBuf::from(&manifest_dir)
        .join("vendor")
        .join("pdfium")
        .join(subdir)
        .join(filename);
    if !src.exists() {
        println!(
            "cargo:warning=ampaper-gui: vendored PDFium not found at {} — run\n\
             curl -L \
             https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-{}.tgz \
             | tar -xz bin/{} && mv bin/{} {}\n\
             or drop the file there manually before building.",
            src.display(),
            // best-effort hint at the right release tarball name
            match (target_os.as_str(), target_arch.as_str()) {
                ("windows", "x86_64") => "win-x64",
                ("linux", "x86_64") => "linux-x64",
                ("macos", "x86_64") => "mac-x64",
                ("macos", "aarch64") => "mac-arm64",
                _ => "<your-target>",
            },
            filename,
            filename,
            src.display(),
        );
        return;
    }

    // OUT_DIR is target/<profile>/build/<crate>-<hash>/out — walk
    // up four levels to land on target/<profile>/. From there we
    // can place the DLL next to ampaper-gui.exe (or the equivalent
    // on Linux/macOS).
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let target_profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR not nested as expected (target/<profile>/build/<crate>-<hash>/out)");
    let dst = target_profile_dir.join(filename);

    // Skip the copy when the destination is already up-to-date —
    // saves a few cycles on every incremental build.
    let needs_copy = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
        (Ok(s), Ok(d)) => s.len() != d.len(),
        (Ok(_), Err(_)) => true,
        (Err(_), _) => false,
    };
    if needs_copy {
        if let Err(e) = std::fs::copy(&src, &dst) {
            println!(
                "cargo:warning=ampaper-gui: failed to copy {} → {}: {e}",
                src.display(),
                dst.display()
            );
        }
    }

    // Tests + examples land in target/<profile>/deps/examples/ and
    // target/<profile>/deps/ respectively. pdfium-render's
    // bind_to_library searches the executable's own directory
    // (current_exe parent), so test and example binaries also need
    // a copy. Ditto for the deps/ folder where unit-test binaries
    // live. Keep them all in sync — small DLLs, cheap copies.
    for sub in ["deps", "examples"] {
        let sub_dst = target_profile_dir.join(sub).join(filename);
        if let Some(parent) = sub_dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let needs = match (std::fs::metadata(&src), std::fs::metadata(&sub_dst)) {
            (Ok(s), Ok(d)) => s.len() != d.len(),
            (Ok(_), Err(_)) => true,
            (Err(_), _) => false,
        };
        if needs {
            let _ = std::fs::copy(&src, &sub_dst);
        }
    }
}
