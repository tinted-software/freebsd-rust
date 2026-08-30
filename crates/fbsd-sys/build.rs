//! Generates raw FFI bindings against a real FreeBSD kernel source tree.
//!
//! `cargo kbuild` (see `crates/cargo-kbuild`) sets `FBSD_SYSDIR` to the
//! `sys/` directory of the reference source tree before invoking `cargo
//! build`; that is the only supported entry point. Running `cargo
//! build`/`check` directly against this crate without `FBSD_SYSDIR` set
//! (e.g. from an editor's rust-analyzer) is not supported and fails fast
//! with a clear error instead of silently producing empty bindings.
//!
//! `FBSD_TARGET_ARCH` selects the FreeBSD `MACHINE`/`MACHINE_ARCH` pair
//! (default `amd64`/`x86_64`) so the right `machine/`, `x86/`, `i386/`
//! (etc.) include-shims can be created, mirroring the `_ILINKS` symlink
//! dance in `sys/conf/kmod.mk`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// (MACHINE, MACHINE_ARCH, extra _ILINKS dirs beyond "machine")
fn arch_links(machine: &str) -> &'static [&'static str] {
    match machine {
        "amd64" => &["x86", "i386"],
        "i386" => &["x86"],
        _ => &[],
    }
}

/// libclang defaults to the host target triple, which lacks x86_64/arm64
/// FreeBSD-specific inline asm constraints and intrinsics used by
/// `<machine/*.h>`; parsing always needs an explicit matching triple.
fn clang_target_triple(machine: &str) -> &'static str {
    match machine {
        "amd64" => "x86_64-unknown-freebsd14",
        "arm64" => "aarch64-unknown-freebsd14",
        "i386" => "i386-unknown-freebsd14",
        other => panic!("no clang target triple mapping for FBSD_MACHINE={other}"),
    }
}

fn main() {
    let sysdir = match env::var("FBSD_SYSDIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            panic!(
                "FBSD_SYSDIR is not set.\n\
                 fbsd-sys must be built via `cargo kbuild`, which points \
                 FBSD_SYSDIR at the `sys/` directory of a FreeBSD source \
                 tree (see ../../README.md)."
            );
        }
    };
    let sysdir = sysdir
        .canonicalize()
        .unwrap_or_else(|e| panic!("FBSD_SYSDIR {sysdir:?} does not exist: {e}"));
    let machine = env::var("FBSD_MACHINE").unwrap_or_else(|_| "amd64".to_string());

    println!("cargo::rerun-if-env-changed=FBSD_SYSDIR");
    println!("cargo::rerun-if-env-changed=FBSD_MACHINE");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ilinks_dir = out_dir.join("ilinks");
    fs::create_dir_all(&ilinks_dir).unwrap();
    make_ilink(
        &ilinks_dir,
        "machine",
        &sysdir.join(&machine).join("include"),
    );
    for extra in arch_links(&machine) {
        make_ilink(&ilinks_dir, extra, &sysdir.join(extra).join("include"));
    }

    let clang_args: Vec<String> = vec![
        format!("--target={}", clang_target_triple(&machine)),
        "-D_KERNEL".into(),
        "-DKLD_MODULE".into(),
        "-nostdinc".into(),
        format!("-I{}", ilinks_dir.display()),
        format!("-I{}", sysdir.display()),
        format!("-I{}/contrib/ck/include", sysdir.display()),
        "-fno-builtin".into(),
        "-D__BSD_VISIBLE=1".into(),
    ];

    let bindings = bindgen::Builder::default()
        .header(wrapper(&out_dir))
        .use_core()
        .ctypes_prefix("::core::ffi")
        .clang_args(&clang_args)
        .layout_tests(false)
        .derive_default(true)
        .derive_debug(false)
        // Kernel headers are full of macros/inline helpers bindgen cannot
        // (and should not) translate; keep the surface to the plain
        // struct/fn/const declarations a KLD actually links against.
        .allowlist_type("moduledata_t|module_t|modeventtype_t|modeventhand_t")
        .allowlist_type("mod_metadata|mod_depend|mod_version|sysinit")
        .allowlist_type("sysinit_sub_id|sysinit_elem_order|sysinit_cfunc_t|sysinit_nfunc_t")
        .allowlist_var("MDT_.*|SI_SUB_.*|SI_ORDER_.*|MOD_LOAD|MOD_UNLOAD|MOD_SHUTDOWN|MOD_QUIESCE")
        .allowlist_var("__FreeBSD_version")
        .allowlist_function("printf|vprintf|panic|module_register_init")
        .generate()
        .expect("failed to generate fbsd-sys bindings from FreeBSD headers");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

fn wrapper(out_dir: &Path) -> String {
    let path = out_dir.join("wrapper.h");
    fs::write(
        &path,
        r#"
#include <sys/param.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/linker.h>
#include <sys/systm.h>
"#,
    )
    .unwrap();
    path.to_str().unwrap().to_string()
}

fn make_ilink(dir: &Path, name: &str, target: &Path) {
    let link = dir.join(name);
    let _ = fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &link)
        .unwrap_or_else(|e| panic!("failed to link {link:?} -> {target:?}: {e}"));
}
