//! Rust equivalent of `share/examples/kld/syscall/module/syscall.c`'s
//! generic module skeleton: prints on load/unload, does nothing else.
#![no_std]
#![no_main]
#![no_builtins]

use fbsd_kernel::kprintln;
use fbsd_kernel::{kernel_module, module::Event};

fn handle(event: Event) -> i32 {
    match event {
        Event::Load => {
            kprintln!("hello: loaded");
            0
        }
        Event::Unload => {
            kprintln!("hello: unloaded");
            0
        }
        // MOD_SHUTDOWN / MOD_QUIESCE: nothing to clean up.
        Event::Shutdown | Event::Quiesce => 0,
    }
}

kernel_module!(hello, handle);
