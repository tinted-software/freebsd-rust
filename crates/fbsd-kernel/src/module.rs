//! Module lifecycle: emits the `moduledata_t` + `mod_metadata` +
//! `mod_depend`(kernel) + `SYSINIT` linker-set entries that
//! `DECLARE_MODULE(name, data, SI_SUB_KLD, SI_ORDER_ANY)` expands to in C
//! (see `sys/sys/module.h`, `sys/sys/kernel.h`, `sys/sys/linker_set.h`),
//! and wires a plain `fn(Event) -> i32` up as the `modeventhand_t`.

use core::ffi::c_int;

pub use fbsd_sys::module_t;

/// Mirrors `modeventtype_t` (`sys/sys/module.h`). The C typedef for
/// `modeventhand_t` takes it as a plain `int`, not the enum type, so the
/// raw handler installed by `kernel_module!` does the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Load,
    Unload,
    Shutdown,
    Quiesce,
}

impl Event {
    /// `None` for any value outside the four defined by
    /// `enum modeventtype` — the kernel never sends one, but every
    /// hand-written `modeventhand_t` in the tree still has a `default:`
    /// arm returning `EOPNOTSUPP`; `kernel_module!` does the same.
    pub fn from_raw(cmd: c_int) -> Option<Event> {
        match cmd as u32 {
            fbsd_sys::modeventtype_MOD_LOAD => Some(Event::Load),
            fbsd_sys::modeventtype_MOD_UNLOAD => Some(Event::Unload),
            fbsd_sys::modeventtype_MOD_SHUTDOWN => Some(Event::Shutdown),
            fbsd_sys::modeventtype_MOD_QUIESCE => Some(Event::Quiesce),
            _ => None,
        }
    }
}

/// `EOPNOTSUPP` (`sys/sys/errno.h`). Not bindgen'd (this crate does not
/// bind `errno.h`) because it is an ABI-stable POSIX value; used as the
/// `kernel_module!` default-arm return, matching every hand-written
/// `modeventhand_t` in the tree.
pub const EOPNOTSUPP: c_int = 45;

/// `roundup(__FreeBSD_version, 100000) - 1` — `MODULE_KERNEL_MAXVER` in
/// `sys/sys/module.h`. A module built on `M.x` loads on any `M.y` with
/// `y >= x`, but not on `M.z` with `z < x`.
pub const fn kernel_maxver() -> c_int {
    let v = fbsd_sys::__FreeBSD_version;
    (v.div_ceil(100000) * 100000 - 1) as c_int
}

/// A kernel linker-set entry is placed once at a fixed address and only
/// ever read back by the kernel (at module-register time); it is never
/// mutated or raced on by our own code. That's the standard justification
/// for asserting `Sync` on the raw-pointer fields bindgen produces
/// (`*const`/`*mut` are conservatively `!Sync`) so they can live in
/// `static`s at all.
#[doc(hidden)]
#[repr(transparent)]
pub struct RacyCell<T>(pub T);
unsafe impl<T> Sync for RacyCell<T> {}

/// Implementation detail of `kernel_module!`; not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use crate::module::{kernel_maxver, Event, RacyCell, EOPNOTSUPP};
    pub use core::ffi::{c_char, c_int, c_void};
    pub use core::ptr;
    pub use fbsd_sys as sys;
}

/// Declares a generic KLD named `$mod_name`, dispatching load/unload/
/// shutdown/quiesce events to `$handler: fn(Event) -> i32` (`0` on
/// success, else a `sys/errno.h` value — the same contract as a C
/// `modeventhand_t`, see `share/examples/kld/syscall/module/syscall.c`).
///
/// Registers at `SI_SUB_KLD`/`SI_ORDER_ANY`, the pair `DECLARE_MODULE`
/// itself uses for modules that aren't a bus driver (`DRIVER_MODULE`,
/// `SI_SUB_DRIVERS`) or a syscall (`SYSCALL_MODULE`, `SI_SUB_SYSCALLS`).
#[macro_export]
macro_rules! kernel_module {
    ($mod_name:ident, $handler:path) => {
        const _: () = {
            use $crate::module::__private::*;

            unsafe extern "C" fn __evhand(
                _module: sys::module_t,
                cmd: c_int,
                _arg: *mut c_void,
            ) -> c_int {
                match Event::from_raw(cmd) {
                    Some(event) => $handler(event),
                    None => EOPNOTSUPP,
                }
            }

            static MODULE_DATA: RacyCell<sys::moduledata_t> = RacyCell(sys::moduledata_t {
                name: ::core::concat!(::core::stringify!($mod_name), "\0").as_ptr()
                    as *const c_char,
                evhand: Some(__evhand),
                priv_: ptr::null_mut(),
            });

            // MODULE_DEPEND(name, kernel, __FreeBSD_version,
            //     __FreeBSD_version, MODULE_KERNEL_MAXVER) — every module
            // implicitly depends on the exact kernel ABI it was built
            // against (sys/sys/module.h).
            static KERNEL_DEPEND: RacyCell<sys::mod_depend> = RacyCell(sys::mod_depend {
                md_ver_minimum: sys::__FreeBSD_version as c_int,
                md_ver_preferred: sys::__FreeBSD_version as c_int,
                md_ver_maximum: kernel_maxver(),
            });

            #[used]
            #[cfg_attr(target_os = "freebsd", link_section = "set_modmetadata_set")]
            static KERNEL_DEPEND_METADATA: RacyCell<&sys::mod_metadata> =
                RacyCell(&sys::mod_metadata {
                    md_version: 1,
                    md_type: sys::MDT_DEPEND as c_int,
                    md_data: &KERNEL_DEPEND.0 as *const sys::mod_depend as *const c_void,
                    md_cval: c"kernel".as_ptr(),
                });

            static MODULE_METADATA: RacyCell<sys::mod_metadata> = RacyCell(sys::mod_metadata {
                md_version: 1,
                md_type: sys::MDT_MODULE as c_int,
                md_data: &MODULE_DATA.0 as *const sys::moduledata_t as *const c_void,
                md_cval: ::core::concat!(::core::stringify!($mod_name), "\0").as_ptr()
                    as *const c_char,
            });

            #[used]
            #[cfg_attr(target_os = "freebsd", link_section = "set_modmetadata_set")]
            static MODULE_METADATA_ENTRY: RacyCell<&sys::mod_metadata> =
                RacyCell(&MODULE_METADATA.0);

            // SYSINIT(name##module, SI_SUB_KLD, SI_ORDER_ANY,
            //     module_register_init, &data) — runs at boot/load time
            // and actually calls `module_register()` for us.
            #[used]
            #[cfg_attr(target_os = "freebsd", link_section = "set_sysinit_set")]
            static SYSINIT_ENTRY: RacyCell<&sys::sysinit> = RacyCell(&sys::sysinit {
                subsystem: sys::sysinit_sub_id_SI_SUB_KLD,
                order: sys::sysinit_elem_order_SI_ORDER_ANY,
                next: sys::sysinit__bindgen_ty_1 {
                    stqe_next: ptr::null_mut(),
                },
                func: Some(sys::module_register_init),
                udata: &MODULE_DATA.0 as *const sys::moduledata_t as *const c_void,
            });
        };
    };
}
