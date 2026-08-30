//! `printf(9)` wrapper. Deliberately minimal: `kprintln!` only accepts a
//! string literal with no `%` conversions (variadic argument formatting is
//! out of scope for the hello-world driver layer; add typed helpers here as
//! real drivers need them, rather than exposing raw variadic FFI).

use core::ffi::c_char;

/// Calls `printf(9)` with a NUL-terminated, argument-free format string.
///
/// # Safety
/// `fmt` must be NUL-terminated and must not contain any `%` conversion
/// specifiers, since no variadic arguments are supplied.
#[inline]
pub unsafe fn raw_printf(fmt: *const c_char) {
    unsafe {
        fbsd_sys::printf(fmt);
    }
}

/// Prints a literal string followed by `\n` via `printf(9)`.
///
/// ```ignore
/// fbsd_kernel::kprintln!("hello, kernel");
/// ```
#[macro_export]
macro_rules! kprintln {
    ($fmt:literal) => {{
        // SAFETY: `concat!($fmt, "\n\0")` is a NUL-terminated `&'static
        // str`; enforcing the "no '%'" contract is the caller's job (see
        // `raw_printf`).
        unsafe {
            $crate::print::raw_printf(
                ::core::concat!($fmt, "\n\0").as_ptr() as *const ::core::ffi::c_char
            )
        }
    }};
}
