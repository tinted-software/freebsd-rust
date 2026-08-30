//! `cargo kbuild ktest`: builds the module (same pipeline as `build`) and
//! then boots it inside a real FreeBSD VM under QEMU to prove it actually
//! `kldload`s, not just links.
//!
//! No FreeBSD host is required: a prebuilt FreeBSD snapshot VM image is
//! downloaded once from `download.freebsd.org` and cached, then booted
//! read-only-base + a disposable qcow2 overlay so repeated runs never
//! touch the cached base image. The module is handed to the guest as a
//! small ISO9660 image (built with `hdiutil`/`xorriso`/`mkisofs`,
//! whichever is available) attached as a virtio-scsi CD-ROM, so the guest
//! sees it as the ordinary `/dev/cd0` — no guest-side driver surprises,
//! `cd9660` is compiled into every stock `GENERIC` kernel.
//!
//! The test itself drives the serial console like a human would at a
//! keyboard (login as root, `mount`, `kldload`, `kldstat -n`,
//! `kldunload`, `poweroff`), matching on `echo`'d markers after each step
//! rather than trying to parse FreeBSD's prompt format, and fails loudly
//! (with the captured console tail) on the first step that doesn't
//! produce its marker in time, or if `panic:` ever appears in the stream.

use crate::build::{self, BuildArgs};
use crate::common::{machine_cpuarch, need_value, run_cmd};
use indicatif::{ProgressBar, ProgressStyle};
use rootcause::option_ext::OptionExt as _;
use rootcause::prelude::ResultExt as _;
use rootcause::{bail, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct KtestArgs {
    build: BuildArgs,
    image_url: Option<String>,
    cache_dir: Option<PathBuf>,
    memory: String,
    smp: String,
    step_timeout: Duration,
    qemu_bin: Option<String>,
    fresh: bool,
    keep: bool,
}

pub fn parse_args(raw: Vec<String>) -> Result<KtestArgs> {
    // arm64 is HVF-accelerated (native) on Apple Silicon hosts and is the
    // architecture the reference VM image URL in this tool's issue was
    // given for; amd64 works too but under TCG emulation on non-x86
    // hosts, which is much slower to boot.
    let mut build_args = build::parse_args_defaults_only("arm64");
    let mut sysdir_set = false;
    let mut image_url = None;
    let mut cache_dir = None;
    let mut memory = "2G".to_string();
    let mut smp = "2".to_string();
    let mut step_timeout = Duration::from_secs(120);
    let mut qemu_bin = None;
    let mut fresh = false;
    let mut keep = false;

    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        if build::try_parse_flag(&a, &mut it, &mut build_args, &mut sysdir_set)? {
            continue;
        }
        match a.as_str() {
            "--image-url" => image_url = Some(need_value(&mut it, "--image-url")?),
            "--cache-dir" => cache_dir = Some(PathBuf::from(need_value(&mut it, "--cache-dir")?)),
            "--memory" => memory = need_value(&mut it, "--memory")?,
            "--smp" => smp = need_value(&mut it, "--smp")?,
            "--timeout" => {
                let secs: u64 = need_value(&mut it, "--timeout")?
                    .parse()
                    .context("--timeout must be a number of seconds")?;
                step_timeout = Duration::from_secs(secs);
            }
            "--qemu" => qemu_bin = Some(need_value(&mut it, "--qemu")?),
            "--fresh" => fresh = true,
            "--keep" => keep = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unrecognized argument: {other} (see --help)"),
        }
    }
    if !sysdir_set {
        bail!(
            "--sysdir <path> is required: point it at a FreeBSD source tree \
             (its root or its sys/ subdirectory)"
        );
    }

    Ok(KtestArgs {
        build: build_args,
        image_url,
        cache_dir,
        memory,
        smp,
        step_timeout,
        qemu_bin,
        fresh,
        keep,
    })
}

fn print_help() {
    println!(
        "cargo kbuild ktest --sysdir <path> [options]\n\n\
         Builds the module (same options as `cargo kbuild build`) and \
         boots it in a QEMU FreeBSD VM to prove it kldloads.\n\n\
         Build options: see `cargo kbuild build --help` (all accepted here too;\n\
         --machine defaults to arm64, since that's HVF-accelerated on Apple\n\
         Silicon hosts).\n\n\
         VM options:\n\
         \x20 --image-url <url>   VM image .xz (default: FreeBSD-CURRENT snapshot\n\
         \x20                     VM-IMAGES for --machine, from download.freebsd.org)\n\
         \x20 --cache-dir <dir>   where the downloaded/decompressed image is cached\n\
         \x20                     (default ~/.cache/opendarwin-kbuild)\n\
         \x20 --memory <size>     QEMU -m value (default 2G)\n\
         \x20 --smp <n>           QEMU -smp value (default 2)\n\
         \x20 --timeout <secs>    per-step console timeout (default 120)\n\
         \x20 --qemu <path>       override the qemu-system-* binary\n\
         \x20 --fresh             re-download/re-decompress the VM image\n\
         \x20 --keep              keep the disk overlay/ISO/log for debugging\n"
    );
}

pub fn run(args: &KtestArgs) -> Result<()> {
    let ko_path = build::build(&args.build)?;
    let ko_path = fs::canonicalize(&ko_path)?;
    let ko_name = ko_path
        .file_name()
        .context("built .ko has no file name")?
        .to_string_lossy()
        .into_owned();
    let module_name = ko_name
        .strip_suffix(".ko")
        .context("built artifact doesn't end in .ko")?
        .to_string();

    let machine = args.build.machine.as_str();
    let _cpuarch = machine_cpuarch(machine)?;
    let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
    fs::create_dir_all(&cache_dir).context("creating --cache-dir")?;

    let image_url = args
        .image_url
        .clone()
        .unwrap_or_else(|| default_image_url(machine).to_string());
    let xz_path = download_cached(&image_url, &cache_dir, args.fresh)?;
    let base_qcow2 = decompress_cached(&xz_path, args.fresh)?;

    let work_dir = args.build.target_dir.join("ktest-work").join(&module_name);
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).context("creating ktest work dir")?;

    let overlay = work_dir.join("overlay.qcow2");
    run_cmd(
        Command::new("qemu-img")
            .arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-F")
            .arg("qcow2")
            .arg("-b")
            .arg(&base_qcow2)
            .arg(&overlay),
    )?;

    let iso_src = work_dir.join("iso-src");
    fs::create_dir_all(&iso_src)?;
    fs::copy(&ko_path, iso_src.join(&ko_name))?;
    let iso = work_dir.join("test.iso");
    build_iso(&iso_src, &iso)?;

    let log_path = work_dir.join("console.log");
    let result = boot_and_test(
        args,
        machine,
        &overlay,
        &iso,
        &ko_name,
        &module_name,
        &log_path,
    );

    if !args.keep {
        let _ = fs::remove_file(&overlay);
        let _ = fs::remove_dir_all(&iso_src);
        let _ = fs::remove_file(&iso);
    } else {
        println!(
            "cargo-kbuild: kept VM artifacts under {}",
            work_dir.display()
        );
    }

    match &result {
        Ok(()) => println!(
            "cargo-kbuild: ktest PASSED ({module_name}.ko loads and unloads cleanly on \
             --machine {machine})"
        ),
        Err(_) => eprintln!(
            "cargo-kbuild: ktest FAILED; full console log: {}",
            log_path.display()
        ),
    }
    result
}

fn default_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("opendarwin-kbuild");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("opendarwin-kbuild");
    }
    PathBuf::from(".kbuild-cache")
}

fn default_image_url(machine: &str) -> &'static str {
    match machine {
        // 15.0-RELEASE, not a `16.0-CURRENT` snapshot: a numbered release
        // freezes `__FreeBSD_version`, so it stays a stable target for
        // `DECLARE_MODULE`'s implicit `MODULE_DEPEND(kernel, ...)` check
        // (see `fbsd_kernel::module::kernel_module!`) across runs, unlike
        // `-CURRENT`, which bumps it frequently. Build against a `--sysdir`
        // checked out at a matching point (e.g. `releng/15.0`/`stable/15`)
        // for `ktest` to actually `kldload`, not just link, cleanly — see
        // the README.
        "arm64" => "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/aarch64/Latest/FreeBSD-15.1-RELEASE-arm64-aarch64-zfs.qcow2.xz",
        "amd64" => "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/amd64/Latest/FreeBSD-15.1-RELEASE-amd64-zfs.qcow2.xz",
        // Reachable only if `machine_cpuarch` grows an entry `ktest` doesn't
        // have a default image for yet.
        other => panic!("no default --image-url for --machine {other}"),
    }
}

/// Downloads `url` into `cache_dir`, streaming to a `.part` file and
/// renaming on success so a half-finished download is never mistaken for
/// a cached one. Returns the cached `.xz` path.
fn download_cached(url: &str, cache_dir: &Path, fresh: bool) -> Result<PathBuf> {
    let filename = url
        .rsplit('/')
        .next()
        .context("--image-url has no path component")?;
    let dest = cache_dir.join(filename);
    if dest.exists() && !fresh {
        println!("cargo-kbuild: using cached {}", dest.display());
        return Ok(dest);
    }

    println!("cargo-kbuild: downloading {url}");
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("building HTTP client")?;
    let mut resp = client
        .get(url)
        .send()
        .context_with(|| format!("GET {url}"))?
        .error_for_status()
        .context_with(|| format!("GET {url}"))?;
    let total = resp.content_length().unwrap_or(0);

    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{wide_bar}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );

    let part = dest.with_extension("part");
    let mut file = File::create(&part).context_with(|| format!("creating {part:?}"))?;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = resp.read(&mut buf).context("reading download stream")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("writing download")?;
        bar.inc(n as u64);
    }
    bar.finish_and_clear();
    drop(file);
    fs::rename(&part, &dest).context_with(|| format!("renaming {part:?} to {dest:?}"))?;
    Ok(dest)
}

/// Decompresses `xz_path` (via the system `xz` binary) into a cached
/// `.qcow2` next to it, skipping the work if it's already there.
fn decompress_cached(xz_path: &Path, fresh: bool) -> Result<PathBuf> {
    let qcow2_path = xz_path.with_extension("");
    if qcow2_path.exists() && !fresh {
        return Ok(qcow2_path);
    }
    println!(
        "cargo-kbuild: decompressing {} (this only happens once)",
        xz_path.display()
    );
    let part = qcow2_path.with_extension("qcow2.part");
    let out_file = File::create(&part).context_with(|| format!("creating {part:?}"))?;
    let status = Command::new("xz")
        .arg("-dc")
        .arg("-T0")
        .arg(xz_path)
        .stdout(Stdio::from(out_file))
        .status()
        .context("running `xz -dc`")?;
    if !status.success() {
        bail!("`xz -dc {xz_path:?}` exited with {status}");
    }
    fs::rename(&part, &qcow2_path)
        .context_with(|| format!("renaming {part:?} to {qcow2_path:?}"))?;
    Ok(qcow2_path)
}

/// Builds an ISO9660 image containing every file in `src_dir`, trying
/// whichever ISO-building tool is available (`xorriso`, `mkisofs`,
/// macOS's built-in `hdiutil`).
fn build_iso(src_dir: &Path, iso_path: &Path) -> Result<()> {
    let _ = fs::remove_file(iso_path);
    if which("xorriso") {
        return run_cmd(
            Command::new("xorriso")
                .arg("-as")
                .arg("mkisofs")
                .arg("-V")
                .arg("KBUILD")
                .arg("-J")
                .arg("-R")
                .arg("-o")
                .arg(iso_path)
                .arg(src_dir),
        );
    }
    if which("mkisofs") {
        return run_cmd(
            Command::new("mkisofs")
                .arg("-V")
                .arg("KBUILD")
                .arg("-J")
                .arg("-R")
                .arg("-o")
                .arg(iso_path)
                .arg(src_dir),
        );
    }
    if which("hdiutil") {
        return run_cmd(
            Command::new("hdiutil")
                .arg("makehybrid")
                .arg("-iso")
                .arg("-joliet")
                .arg("-default-volume-name")
                .arg("KBUILD")
                .arg("-o")
                .arg(iso_path)
                .arg(src_dir),
        );
    }
    bail!("no ISO-building tool found (need one of: xorriso, mkisofs, hdiutil)");
}

fn which(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        || Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin}"))
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn boot_and_test(
    args: &KtestArgs,
    machine: &str,
    overlay: &Path,
    iso: &Path,
    ko_name: &str,
    module_name: &str,
    log_path: &Path,
) -> Result<()> {
    let mut cmd = qemu_command(args, machine, overlay, iso)?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    println!("cargo-kbuild: booting: {cmd:?}");
    let mut child = cmd.spawn().context("spawning qemu")?;
    let mut console = Console::spawn(&mut child, log_path)?;

    let test_result = (|| -> Result<()> {
        console.expect(args.step_timeout, &["login:"])?;
        console.send("root")?;
        // Root has no password on the stock VM-IMAGES; if this particular
        // build does prompt, an empty password is the documented default.
        // Either way, don't block long on this: whatever the shell says
        // next, `echo` a marker and wait for that instead of trying to
        // recognize FreeBSD's prompt format.
        if console
            .expect(Duration::from_secs(8), &["Password:"])
            .is_ok()
        {
            console.send("")?;
        }
        console.send("echo KBUILD_MARK_BOOTED")?;
        console.expect_marker(args.step_timeout, "KBUILD_MARK_BOOTED")?;

        console.send("mount -t cd9660 /dev/cd0 /mnt && echo KBUILD_MARK_MOUNTED")?;
        console.expect_marker(args.step_timeout, "KBUILD_MARK_MOUNTED")?;

        console.send(&format!(
            "kldload /mnt/{ko_name} && echo KBUILD_MARK_LOADED"
        ))?;
        console.expect_marker(args.step_timeout, "KBUILD_MARK_LOADED")?;

        console.send(&format!(
            "kldstat -n {module_name} >/dev/null && echo KBUILD_MARK_PRESENT"
        ))?;
        console.expect_marker(args.step_timeout, "KBUILD_MARK_PRESENT")?;

        console.send(&format!(
            "kldunload {module_name} && echo KBUILD_MARK_UNLOADED"
        ))?;
        console.expect_marker(args.step_timeout, "KBUILD_MARK_UNLOADED")?;

        console.send("poweroff")?;
        Ok(())
    })();

    // Give the guest a chance to actually power off cleanly either way;
    // a stuck/panicked guest gets force-killed so we don't hang forever.
    let exited = wait_with_timeout(&mut child, Duration::from_secs(30));
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    console.join();

    let full_log = fs::read_to_string(log_path).unwrap_or_default();
    if full_log.contains("panic:") {
        bail!(
            "kernel panic detected in console output (see {}):\n{}",
            log_path.display(),
            tail(&full_log, 40)
        );
    }
    Ok(test_result.context_with(|| format!("see full console log at {}", log_path.display()))?)
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn tail(s: &str, lines: usize) -> String {
    let all: Vec<&str> = s.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Drives the QEMU serial console: everything read from the child's
/// stdout is both appended to `log_path` (for post-mortem inspection) and
/// buffered for pattern matching, since FreeBSD's console prompt format
/// isn't worth depending on — every step instead ends with `echo` of a
/// unique marker and `expect` waits for that literal string.
struct Console {
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
    buf: String,
    reader: Option<std::thread::JoinHandle<()>>,
    last_matched: String,
}

impl Console {
    fn spawn(child: &mut Child, log_path: &Path) -> Result<Self> {
        let stdin = child.stdin.take().context("qemu stdin not piped")?;
        let mut stdout = child.stdout.take().context("qemu stdout not piped")?;
        let mut log = File::create(log_path).context_with(|| format!("creating {log_path:?}"))?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = log.write_all(&buf[..n]);
                        let _ = log.flush();
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Console {
            stdin,
            rx,
            buf: String::new(),
            reader: Some(reader),
            last_matched: String::new(),
        })
    }

    /// Blocks until one of `patterns` appears in the accumulated output,
    /// or `timeout` elapses. On success, `self.last_matched` is set to
    /// the pattern that matched.
    fn expect(&mut self, timeout: Duration, patterns: &[&str]) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            for p in patterns {
                if self.buf.contains(p) {
                    self.last_matched = p.to_string();
                    return Ok(());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out after {timeout:?} waiting for {patterns:?}; last output:\n{}",
                    tail(&self.buf, 20)
                );
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(chunk) => self
                    .buf
                    .push_str(&String::from_utf8_lossy(&chunk).replace('\r', "")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "qemu's console closed while waiting for {patterns:?}; last output:\n{}",
                        tail(&self.buf, 20)
                    );
                }
            }
        }
    }

    /// Like [`Self::expect`], but only matches `marker` as the sole
    /// content of an output line (`"\n{marker}\n"`), not as a substring
    /// anywhere. This is required, not cosmetic: the serial console
    /// echoes every keystroke we `send`, so a plain substring search for
    /// `"echo KBUILD_MARK_X"`'s own marker text would immediately
    /// "match" the echoed input line before the command even ran.
    fn expect_marker(&mut self, timeout: Duration, marker: &str) -> Result<()> {
        let needle = format!("\n{marker}\n");
        let deadline = Instant::now() + timeout;
        loop {
            if self.buf.contains(&needle) {
                self.last_matched = marker.to_string();
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out after {timeout:?} waiting for marker {marker:?}; last output:\n{}",
                    tail(&self.buf, 20)
                );
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(chunk) => self
                    .buf
                    .push_str(&String::from_utf8_lossy(&chunk).replace('\r', "")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "qemu's console closed while waiting for marker {marker:?}; last output:\n{}",
                        tail(&self.buf, 20)
                    );
                }
            }
        }
    }

    fn send(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.write_all(b"\n"))
            .context_with(|| format!("writing {line:?} to qemu console"))?;
        Ok(())
    }

    fn join(mut self) {
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

fn qemu_command(args: &KtestArgs, machine: &str, overlay: &Path, iso: &Path) -> Result<Command> {
    let binary_default = match machine {
        "arm64" => "qemu-system-aarch64",
        "amd64" => "qemu-system-x86_64",
        other => bail!("unsupported --machine {other} for ktest"),
    };
    let binary = args
        .qemu_bin
        .clone()
        .unwrap_or_else(|| binary_default.to_string());
    let native = std::env::consts::ARCH
        == if machine == "arm64" {
            "aarch64"
        } else {
            "x86_64"
        };

    let mut cmd = Command::new(binary);
    cmd.arg("-m")
        .arg(&args.memory)
        .arg("-smp")
        .arg(&args.smp)
        .arg("-nographic")
        .arg("-no-reboot")
        // QEMU auto-adds a default NIC (`-nic user`) when none is given;
        // its usermode DHCP server is flaky enough under `-nographic`
        // that `dhclient` can retry/watchdog-timeout for minutes before
        // ever reaching a login prompt. We don't need networking at all
        // (the module is handed over via the CD-ROM), so disable it.
        .arg("-nic")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-monitor")
        .arg("none")
        .arg("-drive")
        .arg(format!(
            "if=none,id=hd0,file={},format=qcow2",
            overlay.display()
        ))
        .arg("-device")
        .arg("virtio-blk-pci,drive=hd0")
        .arg("-device")
        .arg("virtio-scsi-pci,id=scsi0")
        .arg("-drive")
        .arg(format!(
            "if=none,id=cd0,file={},format=raw,readonly=on",
            iso.display()
        ))
        .arg("-device")
        .arg("scsi-cd,bus=scsi0.0,drive=cd0");

    match machine {
        "arm64" => {
            let (code, vars) = aarch64_firmware(&std::env::temp_dir())?;
            cmd.arg("-machine")
                .arg("virt,gic-version=max")
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,file={},readonly=on",
                    code.display()
                ))
                .arg("-drive")
                .arg(format!("if=pflash,format=raw,file={}", vars.display()));
            if native {
                cmd.arg("-accel").arg("hvf").arg("-cpu").arg("host");
            } else {
                cmd.arg("-accel").arg("tcg").arg("-cpu").arg("max");
            }
        }
        "amd64" => {
            cmd.arg("-machine").arg("q35");
            if native {
                cmd.arg("-accel").arg("hvf").arg("-cpu").arg("host");
            } else {
                cmd.arg("-accel").arg("tcg").arg("-cpu").arg("qemu64");
            }
        }
        _ => unreachable!(),
    }
    Ok(cmd)
}

/// Locates the aarch64 UEFI code pflash shipped by the system's QEMU
/// install, and creates a same-sized blank writable vars pflash (aarch64
/// UEFI initializes an all-zero vars store fine; there's no fixed
/// per-arch template to copy, unlike 32-bit arm).
fn aarch64_firmware(work_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let candidates = [
        "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
        "/usr/local/share/qemu/edk2-aarch64-code.fd",
        "/usr/share/qemu/edk2-aarch64-code.fd",
        "/usr/share/edk2/aarch64/QEMU_EFI.fd",
        "/usr/share/AAVMF/AAVMF_CODE.fd",
    ];
    let code = candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .context(
            "no aarch64 UEFI firmware found; install qemu's edk2-aarch64 firmware or pass \
             --qemu with a wrapper that supplies -drive if=pflash yourself",
        )?;
    let size = fs::metadata(&code)?.len();
    let vars = work_dir.join("kbuild-edk2-aarch64-vars.fd");
    let f = File::create(&vars).context_with(|| format!("creating {vars:?}"))?;
    f.set_len(size).context("sizing UEFI vars pflash")?;
    Ok((code, vars))
}
