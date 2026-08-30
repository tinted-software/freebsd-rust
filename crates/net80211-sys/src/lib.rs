//! Raw FFI bindings to `sys/net80211` (`ieee80211com`, `ieee80211vap`,
//! `ieee80211_node`, channel/state constants). Generated at build time
//! from the real headers under `FBSD_SYSDIR` (see `build.rs`). No safe
//! wrapper is provided yet — writing one requires an actual net80211
//! driver to validate the API against, which is future work; this crate
//! only carries the bindings so that work can start without re-deriving
//! the ABI by hand.
#![no_std]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(dead_code, unused)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
