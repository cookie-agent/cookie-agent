use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=COOKIE_GIT_HASH");

    let package_version = env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let version = match env::var("COOKIE_GIT_HASH") {
        Ok(hash) if !hash.is_empty() => format!("{package_version}+{hash}"),
        _ => package_version,
    };

    println!("cargo:rustc-env=COOKIE_VERSION={version}");
}
