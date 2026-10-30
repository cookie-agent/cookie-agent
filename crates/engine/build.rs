use std::env;

/// Passes the release build's identity to the `release` module: the release
/// tag and commit the release workflow sets, empty for every other build,
/// and the target triple.
fn main() {
    for name in ["COOKIE_RELEASE_TAG", "COOKIE_GIT_HASH"] {
        println!("cargo:rerun-if-env-changed={name}");
        let value = env::var(name).unwrap_or_default();
        println!("cargo:rustc-env={name}={value}");
    }
    let target = env::var("TARGET").expect("Cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=COOKIE_TARGET={target}");
}
