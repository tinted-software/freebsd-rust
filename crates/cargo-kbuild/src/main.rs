//! `cargo kbuild`: builds an out-of-tree FreeBSD KLD (`.ko`) from a Rust
//! crate, against a real FreeBSD source tree passed via `--sysdir`.
//!
//! Pipeline (mirrors `sys/conf/kmod.mk`, read against the reference tree
//! while writing this tool):
//!   1. `cargo build --crate-type=staticlib` for a custom
//!      `targets/<machine>-unknown-freebsd-kernel.json` target with
//!      `-Z build-std=core`, `FBSD_SYSDIR`/`FBSD_MACHINE` set so
//!      `fbsd-sys`/`net80211-sys`'s `build.rs` can bindgen the real
//!      headers.
//!   2. `llvm-ar x` the resulting `lib<crate>.a` into its member `.o`s
//!      (equivalent of kmod.mk's per-file `SRCS`/`OBJS`).
//!   3. Partial-link (`ld -r`) those objects with the target's
//!      `sys/conf/ldscript.kmod.<MACHINE>`, exactly like kmod.mk's final
//!      `${LD} -m ${LD_EMULATION} ${LDSCRIPT_FLAGS} -r -o ...` rule.
//!   4. Default `EXPORT_SYMS=NO` symbol localization via the real
//!      `sys/conf/kmod_syms.awk` + `objcopy -L`/`-N`.
//!   5. On non-amd64 (`__KLD_SHARED=yes` in kmod.mk), an extra
//!      `ld -Bshareable` pass turns the partially-linked `.kld` into the
//!      final `.ko`; amd64 links directly.
//!   6. `objcopy --strip-debug` on the final artifact (kmod.mk's default
//!      when `DEBUG_FLAGS` is unset).
//!
//! The actual linking is done with the LLVM tools bundled with the Rust
//! nightly toolchain (`rust-lld`, `llvm-ar`, `llvm-nm`, `llvm-objcopy`),
//! so this works without installing a FreeBSD cross toolchain.

use rootcause::option_ext::OptionExt as _;
use rootcause::prelude::ResultExt as _;
use rootcause::{bail, Result};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Args {
    sysdir: PathBuf,
    machine: String,
    manifest_path: PathBuf,
    release: bool,
    out: Option<PathBuf>,
    export_syms: ExportSyms,
    target_dir: PathBuf,
    extra_cargo_args: Vec<String>,
}

enum ExportSyms {
    /// kmod.mk default: localize every global symbol not required by the
    /// linker sets / not listed.
    No,
    /// Export every global symbol (kmod.mk `EXPORT_SYMS=YES`): skip
    /// localization entirely.
    Yes,
    /// `EXPORT_SYMS=<file>`: only the listed symbols stay global.
    List(PathBuf),
}

fn main() {
    if let Err(e) = run() {
        eprintln!("cargo-kbuild: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    let sysdir = resolve_sysdir(&args.sysdir)?;
    let machine = args.machine.as_str();
    let cpuarch = machine_cpuarch(machine)?;
    let ld_emulation = ld_emulation(machine)?;

    let host = host_triple()?;
    let sysroot = rustc_sysroot()?;
    let tool = |name: &str| -> Result<PathBuf> {
        let p = sysroot
            .join("lib/rustlib")
            .join(&host)
            .join("bin")
            .join(name);
        if !p.exists() {
            bail!(
                "{name} not found at {p:?}; install it with \
                 `rustup component add llvm-tools`"
            );
        }
        Ok(p)
    };
    let rust_lld = tool("rust-lld")?;
    let llvm_ar = tool("llvm-ar")?;
    let llvm_nm = tool("llvm-nm")?;
    let llvm_objcopy = tool("llvm-objcopy")?;

    let target_json = target_spec_path(machine)?;
    let crate_name = read_crate_name(&args.manifest_path)?;
    let profile_dir = if args.release { "release" } else { "debug" };

    // Step 1: cross-compile the crate as a staticlib against the real
    // FreeBSD headers.
    let target_dir = args.target_dir.clone();
    fs::create_dir_all(&target_dir).context("creating --target-dir")?;
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&args.manifest_path)
        .arg("--target")
        .arg(&target_json)
        .arg("-Z")
        .arg("build-std=core")
        .arg("-Z")
        .arg("json-target-spec")
        .arg("--target-dir")
        .arg(&target_dir)
        .env("FBSD_SYSDIR", &sysdir)
        .env("FBSD_MACHINE", machine)
        .args(&args.extra_cargo_args);
    if args.release {
        cmd.arg("--release");
    }
    run_cmd(&mut cmd)?;

    let target_stem = target_json
        .file_stem()
        .context("target json has no file stem")?;
    let staticlib = target_dir
        .join(target_stem)
        .join(profile_dir)
        .join(format!("lib{}.a", crate_name.replace('-', "_")));
    if !staticlib.exists() {
        bail!("expected staticlib at {staticlib:?} was not produced");
    }

    // Step 2: extract member objects.
    let work_dir = target_dir.join("kbuild-work").join(&crate_name);
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).context("creating kbuild work dir")?;
    run_cmd(
        Command::new(&llvm_ar)
            .arg("x")
            .arg(fs::canonicalize(&staticlib)?)
            .current_dir(&work_dir),
    )?;
    let objects: Vec<PathBuf> = fs::read_dir(&work_dir)
        .context("reading extracted objects")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "o"))
        .collect();
    if objects.is_empty() {
        bail!("no .o members extracted from {staticlib:?}");
    }

    // Step 3: partial link (`ld -r`), optionally through the FreeBSD kmod
    // linker script when the reference tree ships one for this MACHINE.
    let kld_path = work_dir.join(format!("{crate_name}.kld"));
    let ldscript = sysdir.join(format!("conf/ldscript.kmod.{machine}"));
    let mut link = Command::new(&rust_lld);
    link.arg("-flavor")
        .arg("gnu")
        .arg("-m")
        .arg(ld_emulation)
        .arg("-warn-common")
        .arg("-d");
    if ldscript.exists() {
        link.arg("-T").arg(&ldscript);
    }
    link.arg("-r").arg("-o").arg(&kld_path).args(&objects);
    run_cmd(&mut link)?;

    // Step 4: EXPORT_SYMS (default NO: localize everything not needed).
    apply_export_syms(
        &args.export_syms,
        &kld_path,
        &sysdir,
        &llvm_nm,
        &llvm_objcopy,
        &work_dir,
    )?;

    // Step 5: amd64 links directly; every other MACHINE_CPUARCH goes
    // through an extra `-Bshareable` pass (kmod.mk's `__KLD_SHARED`).
    let final_path = if cpuarch == "amd64" {
        kld_path.clone()
    } else {
        let shared_path = work_dir.join(format!("{crate_name}.ko"));
        run_cmd(
            Command::new(&rust_lld)
                .arg("-flavor")
                .arg("gnu")
                .arg("-m")
                .arg(ld_emulation)
                .arg("-Bshareable")
                .arg("-znotext")
                .arg("-znorelro")
                .arg("-o")
                .arg(&shared_path)
                .arg(&kld_path),
        )?;
        shared_path
    };

    // Step 6: strip debug info (kmod.mk's default when DEBUG_FLAGS unset).
    run_cmd(
        Command::new(&llvm_objcopy)
            .arg("--strip-debug")
            .arg(&final_path),
    )?;

    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("{crate_name}.ko")));
    fs::copy(&final_path, &out).context_with(|| format!("copying {final_path:?} to {out:?}"))?;
    println!("cargo-kbuild: wrote {}", out.display());
    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut raw: Vec<String> = env::args().skip(1).collect();
    // `cargo kbuild ...` invokes us as `cargo-kbuild kbuild ...`.
    if raw.first().map(String::as_str) == Some("kbuild") {
        raw.remove(0);
    }

    let mut sysdir = None;
    let mut machine = "amd64".to_string();
    let mut manifest_path = PathBuf::from("Cargo.toml");
    let mut release = true;
    let mut out = None;
    let mut export_syms = ExportSyms::No;
    let mut target_dir = PathBuf::from("target/kbuild");
    let mut extra_cargo_args = Vec::new();

    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sysdir" | "-s" => {
                sysdir = Some(PathBuf::from(need_value(&mut it, "--sysdir")?));
            }
            "--machine" | "-m" => {
                machine = need_value(&mut it, "--machine")?;
            }
            "--manifest-path" => {
                manifest_path = PathBuf::from(need_value(&mut it, "--manifest-path")?);
            }
            "--debug" => release = false,
            "--release" => release = true,
            "-o" | "--out" => {
                out = Some(PathBuf::from(need_value(&mut it, "--out")?));
            }
            "--target-dir" => {
                target_dir = PathBuf::from(need_value(&mut it, "--target-dir")?);
            }
            "--export-syms" => {
                let v = need_value(&mut it, "--export-syms")?;
                export_syms = match v.as_str() {
                    "YES" | "yes" => ExportSyms::Yes,
                    "NO" | "no" => ExportSyms::No,
                    _ => ExportSyms::List(PathBuf::from(v)),
                };
            }
            "--" => {
                extra_cargo_args.extend(it.by_ref());
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unrecognized argument: {other} (see --help)"),
        }
    }

    let sysdir = sysdir.context(
        "--sysdir <path> is required: point it at a FreeBSD source tree \
         (its root or its sys/ subdirectory)",
    )?;

    Ok(Args {
        sysdir,
        machine,
        manifest_path,
        release,
        out,
        export_syms,
        target_dir,
        extra_cargo_args,
    })
}

fn need_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    Ok(it.next().context_with(|| format!("{flag} needs a value"))?)
}

fn print_help() {
    println!(
        "cargo kbuild --sysdir <path> [options]\n\n\
         Builds an out-of-tree FreeBSD KLD from the Rust crate at \
         --manifest-path (default ./Cargo.toml).\n\n\
         Options:\n\
         \x20 --sysdir, -s <path>     FreeBSD source tree root or sys/ dir (required)\n\
         \x20 --machine, -m <name>    amd64 (default) | arm64\n\
         \x20 --manifest-path <path>  crate to build (default Cargo.toml)\n\
         \x20 --release / --debug     cargo profile (default --release)\n\
         \x20 -o, --out <file>        output .ko path (default <crate>.ko)\n\
         \x20 --export-syms <v>       NO (default) | YES | <symbol-list-file>\n\
         \x20 --target-dir <dir>      cargo/scratch target dir (default target/kbuild)\n\
         \x20 -- <cargo args...>      passed through to `cargo build`\n"
    );
}

fn resolve_sysdir(p: &Path) -> Result<PathBuf> {
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

fn machine_cpuarch(machine: &str) -> Result<&'static str> {
    Ok(match machine {
        "amd64" => "amd64",
        "arm64" => "aarch64",
        "i386" => "i386",
        other => bail!("unsupported --machine {other} (supported: amd64, arm64)"),
    })
}

fn ld_emulation(machine: &str) -> Result<&'static str> {
    // sys/conf/kern.mk LD_EMULATION_${MACHINE_ARCH}.
    Ok(match machine {
        "amd64" => "elf_x86_64_fbsd",
        "arm64" => "aarch64elf",
        "i386" => "elf_i386_fbsd",
        other => bail!("unsupported --machine {other} (supported: amd64, arm64)"),
    })
}

/// Locates `targets/<machine>-unknown-freebsd-kernel.json` relative to
/// this workspace. `cargo-kbuild` only makes sense run from within (or
/// pointed at) this repo, since that's where the target specs and the
/// `fbsd-sys`/`fbsd-kernel` crates a module depends on live.
fn target_spec_path(machine: &str) -> Result<PathBuf> {
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

fn read_crate_name(manifest_path: &Path) -> Result<String> {
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

fn host_triple() -> Result<String> {
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

fn rustc_sysroot() -> Result<PathBuf> {
    let out = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .context("running `rustc --print sysroot`")?;
    let text = String::from_utf8(out.stdout).context("rustc --print sysroot not UTF-8")?;
    Ok(PathBuf::from(text.trim()))
}

fn apply_export_syms(
    export_syms: &ExportSyms,
    object: &Path,
    sysdir: &Path,
    llvm_nm: &Path,
    llvm_objcopy: &Path,
    work_dir: &Path,
) -> Result<()> {
    if let ExportSyms::Yes = export_syms {
        return Ok(());
    }
    let export_list_path = work_dir.join("export_syms");
    match export_syms {
        ExportSyms::No => fs::write(&export_list_path, "")?,
        ExportSyms::List(path) => {
            let filtered: String = fs::read_to_string(path)
                .context_with(|| format!("reading export-syms list {path:?}"))?
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| format!("{l}\n"))
                .collect();
            fs::write(&export_list_path, filtered)?;
        }
        ExportSyms::Yes => unreachable!(),
    }

    let awk_script = sysdir.join("conf/kmod_syms.awk");
    let out = Command::new("awk")
        .arg("-f")
        .arg(&awk_script)
        .arg(object)
        .arg(&export_list_path)
        .env("NM", llvm_nm)
        .output()
        .context_with(|| format!("running awk -f {awk_script:?}"))?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            awk_script,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let flags: Vec<OsString> = String::from_utf8(out.stdout)
        .context("kmod_syms.awk output not UTF-8")?
        .lines()
        .map(OsString::from)
        .collect();
    if flags.is_empty() {
        return Ok(());
    }
    run_cmd(Command::new(llvm_objcopy).args(&flags).arg(object))
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .context_with(|| format!("spawning {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {status}", cmd.get_program());
    }
    Ok(())
}
