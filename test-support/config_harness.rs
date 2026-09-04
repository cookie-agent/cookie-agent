//! Process-wide user-config isolation for test executables.
//!
//! Rust tests share process environment and may run in parallel, so changing
//! these variables from an individual test is racy. This initializer runs
//! before the test harness starts and gives the whole process one isolated
//! home.

use std::{env, fs, path::Path, process, time::SystemTime};

#[used]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")
)]
#[cfg_attr(target_os = "windows", unsafe(link_section = ".CRT$XCU"))]
#[cfg_attr(
    not(any(target_vendor = "apple", target_os = "windows")),
    unsafe(link_section = ".init_array")
)]
static ISOLATE_USER_CONFIG: extern "C" fn() = {
    extern "C" fn initialize() {
        let timestamp = SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |duration| duration.as_nanos());
        let root = env::temp_dir().join(format!("cookie-agent-test-{}-{timestamp}", process::id()));
        let home = root.join("home");
        let data = root.join("xdg-data");
        let config = root.join("xdg-config");

        for directory in [&home, &data, &config] {
            create_private_directory(directory);
        }

        // SAFETY: this runs before main and therefore before the test harness
        // can create parallel test threads or read process environment.
        unsafe {
            env::set_var("HOME", &home);
            env::set_var("XDG_DATA_HOME", &data);
            env::set_var("XDG_CONFIG_HOME", &config);
            #[cfg(windows)]
            env::set_var("USERPROFILE", &home);
        }
    }

    initialize
};

fn create_private_directory(path: &Path) {
    if let Err(error) = fs::create_dir_all(path) {
        abort_initialization(path, &error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
            abort_initialization(path, &error);
        }
    }
}

fn abort_initialization(path: &Path, error: &std::io::Error) -> ! {
    eprintln!(
        "failed to initialize isolated test directory {}: {error}",
        path.display()
    );
    process::abort();
}
