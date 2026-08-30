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
- `crates/cargo-kbuild` — the `cargo kbuild` subcommand.
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
`kmod.mk` expects for non-amd64 targets. Neither has been tested inside an
actual booted FreeBSD kernel (no FreeBSD VM in this environment); that's
the natural next validation step before trusting it beyond `hello world`.

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
