//! Shared helpers used by both the `build` and `ktest` subcommands:
//! locating the LLVM tools bundled with the Rust nightly toolchain, the
//! per-`MACHINE` tables from `sys/conf/kern.mk`, and the workspace's
//! `targets/*.json` specs.

use rootcause::option_ext::OptionExt as _;
use rootcause::prelude::ResultExt as _;
use rootcause::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The LLVM tools bundled with `rustup`'s nightly toolchain that stand in
/// for a FreeBSD cross toolchain (see the `cargo-kbuild` module doc).
pub struct Toolchain {
    pub rust_lld: PathBuf,
    pub llvm_ar: PathBuf,
    pub llvm_nm: PathBuf,
    pub llvm_objcopy: PathBuf,
}

impl Toolchain {
    pub fn discover() -> Result<Self> {
        let host = host_triple()?;
        let sysroot = rustc_sysroot()?;
        let bin_dir = sysroot.join("lib/rustlib").join(&host).join("bin");
        let tool = |name: &str| -> Result<PathBuf> {
            let p = bin_dir.join(name);
            if !p.exists() {
                bail!(
                    "{name} not found at {p:?}; install it with \
                     `rustup component add llvm-tools`"
                );
            }
            Ok(p)
        };
        Ok(Toolchain {
            rust_lld: tool("rust-lld")?,
            llvm_ar: tool("llvm-ar")?,
            llvm_nm: tool("llvm-nm")?,
            llvm_objcopy: tool("llvm-objcopy")?,
        })
    }
}

/// `MACHINE` -> `MACHINE_CPUARCH` (`sys/conf/kern.mk`); decides whether
/// `kmod.mk`'s direct (`amd64`) or shared (`__KLD_SHARED`, everything
/// else) final-link path applies.
pub fn machine_cpuarch(machine: &str) -> Result<&'static str> {
    Ok(match machine {
        "amd64" => "amd64",
        "arm64" => "aarch64",
        other => bail!("unsupported --machine {other} (supported: amd64, arm64)"),
    })
}

/// `LD_EMULATION_${MACHINE_ARCH}` (`sys/conf/kern.mk`).
pub fn ld_emulation(machine: &str) -> Result<&'static str> {
    Ok(match machine {
        "amd64" => "elf_x86_64_fbsd",
        "arm64" => "aarch64elf",
        other => bail!("unsupported --machine {other} (supported: amd64, arm64)"),
    })
}

/// Locates `targets/<machine>-unknown-freebsd-kernel.json` relative to
/// this workspace. `cargo-kbuild` only makes sense run from within (or
/// pointed at) this repo, since that's where the target specs and the
/// `fbsd-sys`/`fbsd-kernel` crates a module depends on live.
pub fn target_spec_path(machine: &str) -> Result<PathBuf> {
    let arch = match machine {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => bail!("unsupported --machine {other} (supported: amd64, arm64)"),
    };
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("CARGO_MANIFEST_DIR has no grandparent")?;
    let path = workspace_root
        .join("targets")
        .join(format!("{arch}-unknown-freebsd-kernel.json"));
    if !path.exists() {
        bail!("target spec {path:?} not found");
    }
    Ok(path)
}

pub fn resolve_sysdir(p: &Path) -> Result<PathBuf> {
    let candidate = if p.join("conf/kmod.mk").exists() {
        p.to_path_buf()
    } else if p.join("sys/conf/kmod.mk").exists() {
        p.join("sys")
    } else {
        bail!(
            "{p:?} doesn't look like a FreeBSD source tree: expected \
             conf/kmod.mk under it (or under a sys/ subdirectory)"
        );
    };
    Ok(fs::canonicalize(&candidate).context_with(|| format!("canonicalizing {candidate:?}"))?)
}

pub fn read_crate_name(manifest_path: &Path) -> Result<String> {
    let text =
        fs::read_to_string(manifest_path).context_with(|| format!("reading {manifest_path:?}"))?;
    // Deliberately not a full TOML parser: kbuild only needs the `[lib]`
    // name override (crate-type = staticlib always sets one implicitly
    // to `[package].name` otherwise) to find the right `lib*.a`.
    let mut section = "";
    let mut package_name = None;
    let mut lib_name = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match name {
                "package" | "lib" => name,
                _ => "",
            };
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key == "name" {
                let value = value.trim().trim_matches('"').to_string();
                match section {
                    "package" => package_name = Some(value),
                    "lib" => lib_name = Some(value),
                    _ => {}
                }
            }
        }
    }
    Ok(lib_name
        .or(package_name)
        .context_with(|| format!("no [package].name or [lib].name in {manifest_path:?}"))?)
}

pub fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running `rustc -vV`")?;
    let text = String::from_utf8(out.stdout).context("rustc -vV output not UTF-8")?;
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .context("`rustc -vV` did not report a host triple")?)
}

pub fn rustc_sysroot() -> Result<PathBuf> {
    let out = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .context("running `rustc --print sysroot`")?;
    let text = String::from_utf8(out.stdout).context("rustc --print sysroot not UTF-8")?;
    Ok(PathBuf::from(text.trim()))
}

pub fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .context_with(|| format!("spawning {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {status}", cmd.get_program());
    }
    Ok(())
}

pub fn need_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    Ok(it.next().context_with(|| format!("{flag} needs a value"))?)
}
