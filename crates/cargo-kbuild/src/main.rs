//! `cargo kbuild`: builds an out-of-tree FreeBSD KLD from a Rust crate
//! (see `build.rs`), and `cargo kbuild ktest`: also boots it in a real
//! FreeBSD VM under QEMU to prove it `kldload`s (see `ktest.rs`).

mod build;
mod common;
mod ktest;

use std::env;

fn main() {
    if let Err(e) = run() {
        eprintln!("cargo-kbuild: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> rootcause::Result<()> {
    let mut raw: Vec<String> = env::args().skip(1).collect();
    // `cargo kbuild ...` invokes us as `cargo-kbuild kbuild ...`.
    if raw.first().map(String::as_str) == Some("kbuild") {
        raw.remove(0);
    }

    match raw.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            build::print_help();
            Ok(())
        }
        Some("ktest") => {
            raw.remove(0);
            ktest::run(&ktest::parse_args(raw)?)
        }
        Some("build") => {
            raw.remove(0);
            build::build(&build::parse_args(raw, "amd64")?).map(drop)
        }
        // No subcommand keyword: every argument is a `build` flag, e.g.
        // bare `cargo kbuild --sysdir ...`.
        _ => build::build(&build::parse_args(raw, "amd64")?).map(drop),
    }
}
