use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=COOKIE_GIT_HASH");

    let package_version = env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let version = match env::var("COOKIE_GIT_HASH") {
        Ok(hash) if !hash.is_empty() => format!("{package_version}+{hash}"),
        _ => package_version,
    };

    println!("cargo:rustc-env=COOKIE_VERSION={version}");

    // Windows gives the main thread 1 MiB of stack, where Linux and macOS
    // give 8 MiB. A debug build overflowed it before doing anything: clap
    // builds the whole command tree on the main thread's stack while it
    // parses the arguments. Ask for 8 MiB.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => println!("cargo:rustc-link-arg-bins=/STACK:{MAIN_STACK_BYTES}"),
            Ok("gnu") => println!("cargo:rustc-link-arg-bins=-Wl,--stack,{MAIN_STACK_BYTES}"),
            _ => {}
        }
    }
}

/// The main thread's stack on Windows.
const MAIN_STACK_BYTES: u32 = 8 * 1024 * 1024;
