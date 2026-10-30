use std::env;

fn main() {
    // Windows gives a program's main thread a 1 MB stack, where Linux and
    // macOS give 8 MB. Clap builds every subcommand in one function on that
    // thread, whose frame alone outgrew 1 MB in debug builds; reserve as
    // much as the other systems do.
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|target_env| target_env == "msvc");
    if windows && msvc {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
