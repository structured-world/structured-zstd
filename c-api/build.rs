//! Build glue for the C ABI front end:
//! - emits the SONAME / install-name a consumer of upstream `libzstd`
//!   expects, so the cdylib is a true drop-in;
//! - installs the vendored headers under `OUT_DIR/include` for packaging;
//! - generates `libzstd.pc` reporting the vendored upstream version.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Upstream zstd release the vendored headers + reported pkg-config version
/// track. Keep in sync with the tracking comment at the top of each header.
const UPSTREAM_VERSION: &str = "1.5.7";

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // A binary linked against upstream `libzstd.so.1` resolves by SONAME, so
    // the cdylib must advertise that exact name to be substitutable.
    match target_os.as_str() {
        "linux" | "android" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libzstd.so.1");
        }
        "macos" | "ios" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,libzstd.1.dylib");
        }
        _ => {}
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let include_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include");

    // Stage the vendored headers next to the built library so downstream
    // packaging can install them under /usr/include alongside the .so.
    let include_dst = out_dir.join("include");
    fs::create_dir_all(&include_dst).expect("create OUT_DIR/include");
    for header in ["zstd.h", "zdict.h", "zstd_errors.h"] {
        fs::copy(include_src.join(header), include_dst.join(header))
            .unwrap_or_else(|e| panic!("copy vendored header {header}: {e}"));
        println!("cargo:rerun-if-changed=include/{header}");
    }

    // `pkg-config --modversion libzstd` must report the upstream version a
    // consumer's build system pins against.
    let pc = format!(
        "prefix=/usr\n\
         exec_prefix=${{prefix}}\n\
         libdir=${{exec_prefix}}/lib\n\
         includedir=${{prefix}}/include\n\
         \n\
         Name: zstd\n\
         Description: structured-zstd, a libzstd-compatible pure-Rust zstd codec\n\
         URL: https://github.com/structured-world/structured-zstd\n\
         Version: {UPSTREAM_VERSION}\n\
         Libs: -L${{libdir}} -lzstd\n\
         Cflags: -I${{includedir}}\n"
    );
    fs::write(out_dir.join("libzstd.pc"), pc).expect("write libzstd.pc");

    println!("cargo:rerun-if-changed=build.rs");
}
