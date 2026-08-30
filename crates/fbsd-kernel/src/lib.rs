//! Minimal `no_std` driver framework for out-of-tree FreeBSD kernel
//! modules (KLDs) written in Rust, on top of the raw `fbsd-sys` bindings.
//!
//! Only the plumbing every KLD needs is implemented: the module lifecycle
//! (`moduledata_t` + `DECLARE_MODULE`-equivalent linker-set metadata, see
//! `sys/sys/module.h`), `printf(9)`/`panic(9)`, and the `#[panic_handler]`
//! every freestanding crate must provide exactly once. Bus/device
//! attachment, `net80211`, mbufs, etc. are deliberately out of scope here;
//! `net80211-sys` carries the raw bindings for that layer, to be wrapped
//! incrementally as real drivers are ported.
#![no_std]
#![no_builtins]

pub mod module;
pub mod print;

/// Re-export of the raw bindings, for code that needs to drop to FFI.
pub use fbsd_sys as sys;

// `sys/sys/linker_set.h`'s `__MAKE_SET_QV` macro declares a *weak
// reference* to `__start_set_<name>`/`__stop_set_<name>` alongside every
// linker-set *entry* it emits (`__WEAK(__CONCAT(__start_set_,set))`),
// not just where a set is *consumed* (`SET_DECLARE`). That's not
// decorative: both GNU ld and lld only auto-synthesize those boundary
// symbols for a `set_<name>` section when something in the link has an
// undefined (possibly weak) reference to them. C code gets this for
// free from that macro; our `module::__private` linker-set statics
// don't reference anything by that name, so without this, `set_*`
// sections in the final `.ko` have real data but no `__start_`/`__stop_`
// symbols to find them by.
//
// This is invisible on `MACHINE_CPUARCH == amd64`: FreeBSD loads a plain
// `ld -r` kld as `link_elf_obj.c`, which locates linker sets by *section
// name* directly, boundary symbols or not. Every other arch's kld is a
// `-Bshareable` shared object loaded by `link_elf.c`, which only knows
// how to find a set via `__start_set_<name>`/`__stop_set_<name>` — no
// weak reference, no symbol, `linker_file_lookup_set()` fails silently,
// and `DECLARE_MODULE`'s `SYSINIT` (and thus the module's `MOD_LOAD`
// event) is never run, even though `module_register()` (a plain function
// call, not a linker-set walk) still succeeds and `kldstat` still lists
// the module. Confirmed exactly this failure mode under qemu-system-aarch64.
core::arch::global_asm!(
    ".weak __start_set_sysinit_set",
    ".weak __stop_set_sysinit_set",
    ".weak __start_set_modmetadata_set",
    ".weak __stop_set_modmetadata_set",
);

#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo) -> ! {
    // panic(9) is declared `__dead2` (never returns): it prints the
    // message, optionally drops into ddb, then halts/reboots. We pass no
    // formatting because the incoming `&str` from `PanicInfo` is not
    // NUL-terminated and printf(9)-style `%s` requires one.
    unsafe { sys::panic(c"rust kernel module panicked".as_ptr()) }
}
