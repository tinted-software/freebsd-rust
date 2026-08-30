//! Bindgen driver for `sys/net80211`. Mirrors `fbsd-sys/build.rs`; see
//! that file for the `FBSD_SYSDIR`/`FBSD_MACHINE` contract. Kept as a
//! separate crate (rather than folded into `fbsd-sys`) because net80211's
//! surface is large and only needed once real 802.11 drivers land; the
//! hello-world example does not depend on it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn arch_links(machine: &str) -> &'static [&'static str] {
    match machine {
        "amd64" => &["x86", "i386"],
        "i386" => &["x86"],
        _ => &[],
    }
}

/// See `fbsd-sys/build.rs`: libclang must be told the real target triple
/// or `<machine/*.h>`'s inline asm/intrinsics fail to parse.
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
                "FBSD_SYSDIR is not set; net80211-sys must be built via \
                 `cargo kbuild` (see ../../README.md)."
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
        // net80211's public surface: the ic/vap/node structures and the
        // ioctl/state-machine constants a driver attaches through. This
        // list grows as real drivers are ported; it is intentionally not
        // "allow everything" because most of net80211_var.h is internal
        // stack state a driver never touches directly.
        .allowlist_type("ieee80211com|ieee80211vap|ieee80211_node|ieee80211_channel")
        .allowlist_type("ieee80211_phytype|ieee80211_opmode|ieee80211_state")
        .allowlist_var("IEEE80211_.*")
        .generate()
        .expect("failed to generate net80211-sys bindings from FreeBSD headers");

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
#include <sys/systm.h>
#include <sys/socket.h>
#include <sys/mbuf.h>
#include <sys/lock.h>
#include <sys/mutex.h>
#include <sys/rwlock.h>
#include <sys/sysctl.h>
#include <sys/taskqueue.h>
#include <sys/counter.h>
#include <net/if.h>
#include <net/if_var.h>
#include <net/if_media.h>
#include <net80211/ieee80211_var.h>
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
