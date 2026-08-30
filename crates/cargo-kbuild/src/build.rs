//! `cargo kbuild build` (also the default when no subcommand is given):
//! builds an out-of-tree FreeBSD KLD (`.ko`) from a Rust crate, against a
//! real FreeBSD source tree passed via `--sysdir`.
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

use crate::common::{
    ld_emulation, machine_cpuarch, need_value, read_crate_name, resolve_sysdir, run_cmd,
    target_spec_path, Toolchain,
};
use rootcause::option_ext::OptionExt as _;
use rootcause::prelude::ResultExt as _;
use rootcause::{bail, Result};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct BuildArgs {
    pub sysdir: PathBuf,
    pub machine: String,
    pub manifest_path: PathBuf,
    pub release: bool,
    pub out: Option<PathBuf>,
    pub export_syms: ExportSyms,
    pub target_dir: PathBuf,
    pub extra_cargo_args: Vec<String>,
}

pub enum ExportSyms {
    /// kmod.mk default: localize every global symbol not required by the
    /// linker sets / not listed.
    No,
    /// Export every global symbol (kmod.mk `EXPORT_SYMS=YES`): skip
    /// localization entirely.
    Yes,
    /// `EXPORT_SYMS=<file>`: only the listed symbols stay global.
    List(PathBuf),
}

impl BuildArgs {
    fn defaults(default_machine: &str) -> Self {
        BuildArgs {
            sysdir: PathBuf::new(),
            machine: default_machine.to_string(),
            manifest_path: PathBuf::from("Cargo.toml"),
            release: true,
            out: None,
            export_syms: ExportSyms::No,
            target_dir: PathBuf::from("target/kbuild"),
            extra_cargo_args: Vec::new(),
        }
    }
}

/// Exposes [`BuildArgs::defaults`] to other subcommands (`ktest`) that
/// need a `BuildArgs` to parse shared flags into before adding their own.
pub fn parse_args_defaults_only(default_machine: &str) -> BuildArgs {
    BuildArgs::defaults(default_machine)
}

/// Parses one `build`-pipeline flag out of `it` into `args`, if `flag`
/// names one. Shared between the `build` and `ktest` subcommands so both
/// accept the same crate-selection/build options from a single
/// implementation. Returns `false` (leaving `it` untouched beyond the
/// flag itself) when `flag` isn't a build flag, so callers can fall
/// through to their own flags.
pub fn try_parse_flag(
    flag: &str,
    it: &mut impl Iterator<Item = String>,
    args: &mut BuildArgs,
    sysdir_set: &mut bool,
) -> Result<bool> {
    match flag {
        "--sysdir" | "-s" => {
            args.sysdir = PathBuf::from(need_value(it, "--sysdir")?);
            *sysdir_set = true;
        }
        "--machine" | "-m" => args.machine = need_value(it, "--machine")?,
        "--manifest-path" => args.manifest_path = PathBuf::from(need_value(it, "--manifest-path")?),
        "--debug" => args.release = false,
        "--release" => args.release = true,
        "-o" | "--out" => args.out = Some(PathBuf::from(need_value(it, "--out")?)),
        "--target-dir" => args.target_dir = PathBuf::from(need_value(it, "--target-dir")?),
        "--export-syms" => {
            let v = need_value(it, "--export-syms")?;
            args.export_syms = match v.as_str() {
                "YES" | "yes" => ExportSyms::Yes,
                "NO" | "no" => ExportSyms::No,
                _ => ExportSyms::List(PathBuf::from(v)),
            };
        }
        "--" => args.extra_cargo_args.extend(it.by_ref()),
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn parse_args(raw: Vec<String>, default_machine: &str) -> Result<BuildArgs> {
    let mut args = BuildArgs::defaults(default_machine);
    let mut sysdir_set = false;
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        if try_parse_flag(&a, &mut it, &mut args, &mut sysdir_set)? {
            continue;
        }
        if a == "-h" || a == "--help" {
            print_help();
            std::process::exit(0);
        }
        bail!("unrecognized argument: {a} (see --help)");
    }
    if !sysdir_set {
        bail!(
            "--sysdir <path> is required: point it at a FreeBSD source tree \
             (its root or its sys/ subdirectory)"
        );
    }
    Ok(args)
}

pub fn print_help() {
    println!(
        "cargo kbuild [build] --sysdir <path> [options]\n\n\
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
         \x20 -- <cargo args...>      passed through to `cargo build`\n\n\
         See `cargo kbuild ktest --help` to also boot the module in QEMU.\n"
    );
}

/// Runs the full pipeline and returns the path the final `.ko` was
/// written to (`args.out`, or `<crate>.ko` in the current directory).
pub fn build(args: &BuildArgs) -> Result<PathBuf> {
    let sysdir = resolve_sysdir(&args.sysdir)?;
    let machine = args.machine.as_str();
    let cpuarch = machine_cpuarch(machine)?;
    let ld_emu = ld_emulation(machine)?;
    let tools = Toolchain::discover()?;

    let target_json = target_spec_path(machine)?;
    let crate_name = read_crate_name(&args.manifest_path)?;
    let profile_dir = if args.release { "release" } else { "debug" };

    // Step 1: cross-compile the crate as a staticlib against the real
    // FreeBSD headers.
    fs::create_dir_all(&args.target_dir).context("creating --target-dir")?;
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
        .arg(&args.target_dir)
        .env("FBSD_SYSDIR", &sysdir)
        .env("FBSD_MACHINE", machine)
        .args(&args.extra_cargo_args);
    if machine == "arm64" {
        // sys/conf/kern.mk: `CFLAGS += -mbranch-protection=standard` for
        // aarch64 (BTI + PAC-RET), applied to every kernel/kmod object.
        // A callee compiled without a BTI landing pad, indirectly called
        // (e.g. our SYSINIT func, or `modeventhand_t` via `MOD_EVENT`)
        // faults with a genuine kernel panic ("Branch Target exception",
        // `sys/arm64/arm64/trap.c`'s `EXCP_BTI` case) the instant the
        // kernel jumps to it — reproduced and confirmed fixed by this.
        // `-Z branch-protection` is an rustc, not cargo, unstable flag,
        // so it has to go through RUSTFLAGS rather than cargo's own argv.
        let mut rustflags = env::var("RUSTFLAGS").unwrap_or_default();
        if !rustflags.is_empty() {
            rustflags.push(' ');
        }
        rustflags.push_str("-Z branch-protection=bti,pac-ret");
        cmd.env("RUSTFLAGS", rustflags);
    }
    if args.release {
        cmd.arg("--release");
    }
    run_cmd(&mut cmd)?;

    let target_stem = target_json
        .file_stem()
        .context("target json has no file stem")?;
    let staticlib = args
        .target_dir
        .join(target_stem)
        .join(profile_dir)
        .join(format!("lib{}.a", crate_name.replace('-', "_")));
    if !staticlib.exists() {
        bail!("expected staticlib at {staticlib:?} was not produced");
    }

    // Step 2: extract member objects.
    let work_dir = args.target_dir.join("kbuild-work").join(&crate_name);
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).context("creating kbuild work dir")?;
    run_cmd(
        Command::new(&tools.llvm_ar)
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
    let mut link = Command::new(&tools.rust_lld);
    link.arg("-flavor")
        .arg("gnu")
        .arg("-m")
        .arg(ld_emu)
        .arg("-warn-common")
        .arg("-d");
    if ldscript.exists() {
        link.arg("-T").arg(&ldscript);
    }
    link.arg("-r").arg("-o").arg(&kld_path).args(&objects);
    run_cmd(&mut link)?;

    // Step 4: EXPORT_SYMS (default NO: localize everything not needed).
    apply_export_syms(&args.export_syms, &kld_path, &sysdir, &tools, &work_dir)?;

    // Step 5: amd64 links directly; every other MACHINE_CPUARCH goes
    // through an extra `-Bshareable` pass (kmod.mk's `__KLD_SHARED`).
    let final_path = if cpuarch == "amd64" {
        kld_path.clone()
    } else {
        let shared_path = work_dir.join(format!("{crate_name}.ko"));
        run_cmd(
            Command::new(&tools.rust_lld)
                .arg("-flavor")
                .arg("gnu")
                .arg("-m")
                .arg(ld_emu)
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
        Command::new(&tools.llvm_objcopy)
            .arg("--strip-debug")
            .arg(&final_path),
    )?;

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{crate_name}.ko")));
    fs::copy(&final_path, &out).context_with(|| format!("copying {final_path:?} to {out:?}"))?;
    println!("cargo-kbuild: wrote {}", out.display());
    Ok(out)
}

fn apply_export_syms(
    export_syms: &ExportSyms,
    object: &Path,
    sysdir: &Path,
    tools: &Toolchain,
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
        .env("NM", &tools.llvm_nm)
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
    run_cmd(Command::new(&tools.llvm_objcopy).args(&flags).arg(object))
}
