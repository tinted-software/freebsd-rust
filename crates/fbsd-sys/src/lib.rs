//! Raw, unsafe FFI bindings to the subset of the FreeBSD kernel ABI needed
//! to build a KLD: `moduledata_t`, `struct sysinit`, `struct mod_metadata`,
//! `printf(9)`, `panic(9)`. Generated at build time by `build.rs` via
//! `bindgen` against the real headers pointed to by `FBSD_SYSDIR`
//! (see `cargo kbuild`). Do not depend on this crate directly from driver
//! code; use the safe wrappers and macros in `fbsd-kernel`.
#![no_std]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(dead_code, unused)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
