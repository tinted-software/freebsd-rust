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

#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo) -> ! {
    // panic(9) is declared `__dead2` (never returns): it prints the
    // message, optionally drops into ddb, then halts/reboots. We pass no
    // formatting because the incoming `&str` from `PanicInfo` is not
    // NUL-terminated and printf(9)-style `%s` requires one.
    unsafe { sys::panic(c"rust kernel module panicked".as_ptr()) }
}
