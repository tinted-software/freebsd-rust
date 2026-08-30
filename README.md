# freebsd-rust

Rust driver layer for the FreeBSD kernel, built out-of-tree against a
regular FreeBSD source checkout (e.g. `../freebsd-src`). Ships a `cargo
kbuild` subcommand that cross-compiles a `no_std` Rust crate and links it
into a real, loadable `.ko` using the same conventions as
`sys/conf/kmod.mk`, without needing a FreeBSD cross toolchain — only the
LLVM tools bundled with `rustup`'s nightly (`rust-lld`, `llvm-ar`,
`llvm-nm`, `llvm-objcopy`).

## Layout

- `targets/*-unknown-freebsd-kernel.json` — freestanding (`no_std`,
  `panic=abort`, no red zone, kernel code model) custom Rust target specs,
  one per supported `MACHINE` (`amd64`, `arm64`).
- `crates/fbsd-sys` — raw `bindgen` FFI bindings to the minimal kernel
  surface every KLD needs: `moduledata_t`, `mod_metadata`/`mod_depend`,
  `struct sysinit`, `printf(9)`, `panic(9)` (`sys/sys/module.h`,
  `kernel.h`, `linker.h`, `systm.h`). Generated at build time against the
  real headers under `FBSD_SYSDIR`.
- `crates/fbsd-kernel` — the driver framework: the `kernel_module!` macro
  (a Rust `DECLARE_MODULE(name, data, SI_SUB_KLD, SI_ORDER_ANY)`
  equivalent, emitting the same `set_modmetadata_set`/`set_sysinit_set`
  linker-set entries `sys/sys/linker_set.h` describes), `kprintln!`, and
  the crate's one required `#[panic_handler]`.
- `crates/net80211-sys` — raw bindings to `sys/net80211`
  (`ieee80211com`/`ieee80211vap`/`ieee80211_node`/channel & state
  constants). No safe wrapper yet; real 802.11 drivers are future work,
  this just carries the ABI so that work doesn't start by re-deriving it
  by hand.
- `crates/cargo-kbuild` — the `cargo kbuild` subcommand (`build`, and `ktest`).
- `examples/hello` — a `hello, kernel`/`hello: unloaded` KLD; the Rust
  equivalent of `share/examples/kld/syscall/module/syscall.c`'s generic
  module skeleton.

## Building a module

```sh
cargo kbuild --sysdir ../freebsd-src -p examples/hello
# or, from within a module crate's directory:
cd examples/hello && cargo kbuild --sysdir ../../../freebsd-src
```

`--sysdir` takes either a FreeBSD source tree root or its `sys/`
subdirectory directly. This writes `hello.ko` in the current directory.

Other flags: `--machine amd64|arm64` (default `amd64`), `--manifest-path`,
`--debug` (default is `--release`), `-o/--out <file>`, `--export-syms
NO|YES|<file>` (default `NO`, matching `kmod.mk`: every global symbol not
required by the module is localized via the real
`sys/conf/kmod_syms.awk`), `--target-dir`, and `-- <args>` passed through
to the underlying `cargo build`. Run `cargo kbuild --help` for the full
list.

### What it actually does

1. `cargo build --target targets/<machine>-unknown-freebsd-kernel.json -Z
build-std=core` for the module crate (`crate-type = ["staticlib"]`),
   with `FBSD_SYSDIR`/`FBSD_MACHINE` set so `fbsd-sys`/`net80211-sys`'s
   `build.rs` bindgen the real headers under `--sysdir` (with the same
   `machine`/`x86`/`i386` include-shims as `kmod.mk`'s `_ILINKS`).
2. `llvm-ar x` the produced `lib<crate>.a` into its member objects — the
   Rust-crate equivalent of `kmod.mk`'s per-file `OBJS`.
3. Partial-link (`ld -r`) those objects through
   `sys/conf/ldscript.kmod.<MACHINE>`, exactly like `kmod.mk`'s
   `${LD} -m ${LD_EMULATION} ${LDSCRIPT_FLAGS} -r -o ...` rule.
4. Localize symbols per `EXPORT_SYMS` (default `NO`) using the real
   `sys/conf/kmod_syms.awk` + `objcopy -L`/`-N`.
5. `amd64` links directly to the final `.ko`; every other `MACHINE_CPUARCH`
   goes through the extra `ld -Bshareable` pass `kmod.mk` calls
   `__KLD_SHARED`.
6. `objcopy --strip-debug` the result.

This has been validated end-to-end against a real FreeBSD source tree: the
produced `hello.ko` for `amd64` is a relocatable ELF with a single-entry
`set_sysinit_set` and a two-entry `set_modmetadata_set` (the module's own
metadata plus its implicit `kernel` dependency), and its only remaining
undefined symbols are genuine kernel/libkern exports (`printf`, `panic`,
`module_register_init`, `memcmp`, `memcpy`, `memset`) — not
`kldload`-time surprises. `arm64` produces a shared-object `.ko` as
`kmod.mk` expects for non-amd64 targets. `cargo kbuild ktest` (below)
confirms both actually `kldload` in a real kernel, not just link cleanly.

## Testing a module in a real kernel: `cargo kbuild ktest`

```sh
cd examples/hello && cargo kbuild ktest --sysdir ../../../freebsd-src
```

Builds the module (same flags as `cargo kbuild build` — `--machine`
defaults to `arm64` here, since that's HVF-accelerated on Apple Silicon
hosts), then boots a real FreeBSD VM under QEMU, `kldload`s the module,
`kldstat -n`s it to confirm it's actually registered, `kldunload`s it,
and powers off — failing loudly (with the full console log) on the first
step that doesn't complete, or if `panic:` ever appears.

No FreeBSD host or manual VM setup needed: the first run downloads and
caches (`~/.cache/opendarwin-kbuild` by default) a
`download.freebsd.org` **15.0-RELEASE** VM-IMAGES image (`--image-url` to
override; a `--machine amd64` default is included too, but expect it to
be much slower under TCG on a non-x86 host — arm64 gets QEMU's `hvf`
accelerator on Apple Silicon). Networking is disabled (`-nic none`): the
module is handed over via a CD-ROM, not the network, and QEMU's default
usermode NIC/DHCP is flaky enough under `-nographic` to stall boot for
minutes on `watchdog timeout on queue 0`/`No DHCPOFFERS received`. Every
run boots a disposable `qcow2` overlay on top of the cached base image,
so the cache is never mutated, and hands the module to the guest as a
small ISO9660 image (built with `hdiutil`/`xorriso`/`mkisofs`, whichever
is available) attached as a virtio-SCSI CD-ROM — `cd9660` and
virtio-scsi are both compiled into every stock `GENERIC` kernel, so no
guest-side driver setup is needed either. The console is driven like a
human typing at it (login as root, `mount`, `kldload`, ...), matching
each step's own `echo`'d marker on its own output line rather than
trying to parse FreeBSD's prompt format.

**A numbered release, not a `-CURRENT` snapshot, on purpose.**
`kernel_module!`'s implicit `MODULE_DEPEND(kernel, ...)` (`sys/sys/module.h`)
only loads on a running kernel whose `__FreeBSD_version` is `>=` the
`--sysdir` tree's version and within the same "hundred-thousand" bucket
(see `fbsd_kernel::module::kernel_maxver`) — by design, this is the same
check every real KLD is subject to. `-CURRENT`'s `__FreeBSD_version`
bumps frequently, so a `--sysdir` checkout even a few days newer than a
cached/default snapshot will legitimately fail to `kldload` with `KLD
hello.ko: depends on kernel - not available or version mismatch` — not a
bug, just version skew. A numbered release freezes its
`__FreeBSD_version`, so check out `--sysdir` at a matching point
(`releng/15.0`/`stable/15` for the `15.0-RELEASE` default) and the
version check stays satisfied run over run.

**Two real bugs found and fixed building this, both `arm64`-only** (the
whole point of testing under an emulator, not just on the one physical
board at hand):

1. `hello.ko` linked cleanly and `kldstat` listed it as loaded on every
   arch, but its `kprintln!("hello: loaded")` never actually ran on
   `arm64` under QEMU — confirmed via `dmesg`, not just a missing console
   echo. Root cause: `sys/conf/kmod.mk` links `amd64` klds directly as
   `ET_REL` objects (loaded by `link_elf_obj.c`, which finds a linker set
   by _section name_), but every other arch links a `-Bshareable`
   _shared object_ (loaded by `link_elf.c`, which finds a linker set only
   via `__start_set_<name>`/`__stop_set_<name>` _symbols_). C code gets
   those symbols for free — `sys/sys/linker_set.h`'s `__MAKE_SET_QV`
   macro declares a weak reference to them next to every set entry it
   emits, which is what makes `ld`/`lld` synthesize the boundary symbols
   in the final link. Our `kernel_module!`-emitted statics never
   referenced anything by that name, so on `arm64` the `sysinit_set`
   lookup silently failed, `DECLARE_MODULE`'s `SYSINIT` (and therefore
   the module's `MOD_LOAD` event) never ran, and `kldstat` still showed
   the module loaded anyway — `module_register()`'s bookkeeping is a
   separate, unconditional step from the `SYSINIT`, so it isn't proof
   `MOD_LOAD` fired. **Fixed** in `fbsd-kernel`'s `lib.rs` with
   `core::arch::global_asm!` emitting the same `.weak
__start_set_sysinit_set` (etc.) declarations C gets automatically.

2. With (1) fixed, `arm64` `kldload` got further — and immediately
   panicked: `panic: Branch Target exception` (`sys/arm64/arm64/trap.c`'s
   `EXCP_BTI` case), the instant the kernel made an indirect call into
   our code (`MOD_EVENT`'s call through `mod->handler`, i.e. our
   `modeventhand_t`). Root cause: `sys/conf/kern.mk` builds every arm64
   kernel/kmod object with `-mbranch-protection=standard` (BTI + PAC-RET)
   — an indirect-branch target without a `BTI` landing pad instruction
   traps on real ARMv8.5-BTI hardware, and QEMU's `virt`/`-cpu host`
   enforces it too. `targets/aarch64-unknown-freebsd-kernel.json` didn't
   request it, so our Rust functions had no landing pads at all. **Fixed**
   in `crates/cargo-kbuild/src/build.rs`: `--machine arm64` builds now set
   `RUSTFLAGS=-Z branch-protection=bti,pac-ret` (the rustc equivalent of
   `-mbranch-protection=standard`); verified via `llvm-objdump -d` that
   `hello.ko` now has `bti c` landing pads and `paciasp`/`autiasp`
   PAC-RET instructions throughout, plus the `.note.gnu.property` ELF
   marker the kernel's loader checks for BTI eligibility.

**Confirmed fully working end to end** after both fixes, on a
version-matched `15.x-RELEASE` image: `cargo kbuild ktest` on
`examples/hello --machine arm64` boots, logs in, mounts the ISO,
`kldload`s `hello.ko` (console shows the module's own `hello: loaded`),
confirms it via `kldstat -n hello`, `kldunload`s it (console shows
`hello: unloaded`), and powers off — no panics, full round trip in well
under a minute once the VM image is cached. `amd64` was independently
confirmed working, `kprintln!` output and all, on real hardware
throughout (unaffected by either bug: no shared-object linking, no
branch-protection requirement).

`ktest`-specific flags: `--image-url`, `--cache-dir`, `--memory` (default
`2G`), `--smp` (default `2`), `--timeout` (per-step console timeout,
default `120`s), `--qemu` (override the `qemu-system-*` binary),
`--fresh` (force re-download/re-decompress), `--keep` (keep the disk
overlay/ISO/console log for debugging — printed path on exit). Run
`cargo kbuild ktest --help` for the full list, and all `build` flags
(`--sysdir`, `--machine`, `--manifest-path`, `--release`/`--debug`,
`--export-syms`, `--target-dir`, `-- <cargo args>`) are accepted too.

## Writing a module

```rust
#![no_std]
#![no_main]
#![no_builtins] // required: see note below

use fbsd_kernel::kprintln;
use fbsd_kernel::module::{kernel_module, Event};

fn handle(event: Event) -> i32 {
    match event {
        Event::Load => { kprintln!("hello: loaded"); 0 }
        Event::Unload => { kprintln!("hello: unloaded"); 0 }
        Event::Shutdown | Event::Quiesce => 0,
    }
}

kernel_module!(hello, handle);
```

`handle` follows the C `modeventhand_t` contract: return `0` on success or
a `sys/errno.h` value on failure.

`#![no_builtins]` is required on every crate that calls `kprintln!`/
`raw_printf`: without it, LLVM recognizes `printf("literal\n")` as libc's
`printf` and rewrites it to a call to `puts`, which the FreeBSD kernel
does not export — an instant `kldload` failure. `fbsd-kernel` itself sets
`#![no_builtins]`, but the attribute is per-crate and does not propagate
through cross-crate inlining, so the module crate needs it too (see
`examples/hello`).

`kprintln!` only accepts a string literal with no `%` (no variadic
argument formatting yet — out of scope for the hello-world layer; add
typed helpers in `fbsd_kernel::print` as real drivers need them instead of
exposing raw variadic FFI).

## Requirements

- `rustup` nightly with the `rust-src` and `llvm-tools` components (see
  `rust-toolchain.toml`; already pinned for this workspace).
- `clang` (any recent one; used by `bindgen`'s `libclang`, not for actual
  compilation — no FreeBSD cross toolchain is required).
- `awk` (for the `EXPORT_SYMS` step, matching `kmod.mk` exactly).

## Supported architectures

`amd64` and `arm64` (`sys/conf/kern.mk`'s `LD_EMULATION_{amd64,aarch64}`
and `sys/conf/ldscript.kmod.{amd64,arm64}`). Adding another `MACHINE`
means adding a `targets/<arch>-unknown-freebsd-kernel.json` and extending
`machine_cpuarch`/`ld_emulation`/`target_spec_path` in
`crates/cargo-kbuild/src/main.rs`.

## Non-goals (for now)

- No safe `net80211` wrapper — `net80211-sys` only carries bindings.
- No `alloc` support (no kernel `GlobalAlloc` backed by `malloc(9)`/
  `M_DEVBUF` yet) — `-Z build-std=core` only.
- No bus/device attachment (`DRIVER_MODULE`, `device_t`, newbus) —
  `kernel_module!` only covers the generic `DECLARE_MODULE` path used by
  standalone modules, not attached drivers.
- `kprintln!` has no `printf`-style formatting.
