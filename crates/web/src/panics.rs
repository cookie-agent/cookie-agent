//! Panics, made visible. The page is built with `panic = "abort"`, so a
//! panic is a WebAssembly trap whose console line ("RuntimeError:
//! unreachable") says nothing of what failed. The hook `install` sets
//! first logs the panic's message and location, with the JavaScript
//! stack, to `console.error`.

use std::fmt::Write as _;

/// What the hook logs for a panic whose `Display` text is `panic` (it
/// starts `panicked at <file>:<line>:<column>:`), with the JavaScript
/// stack `stack` when the browser gives one.
pub fn report(panic: &str, stack: Option<&str>) -> String {
    let mut text = format!("cookie web client {panic}");
    if let Some(stack) = stack.map(str::trim).filter(|stack| !stack.is_empty()) {
        let _ = write!(text, "\n\nStack:\n{stack}");
    }
    text
}

/// Installs the hook; the page's entry point calls it before anything can
/// panic.
#[cfg(target_arch = "wasm32")]
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let stack = StackError::new().stack();
        console_error(&report(&info.to_string(), Some(&stack)));
    }));
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);

    /// A JavaScript `Error`, for the stack it records.
    type StackError;
    #[wasm_bindgen(constructor, js_class = Error)]
    fn new() -> StackError;
    #[wasm_bindgen(method, getter, structural)]
    fn stack(this: &StackError) -> String;
}

/// Panics on purpose, for the browser suite to check that a panic reaches
/// the console with its message (scripts/web-e2e/scenarios.py). Nothing on
/// the page calls it.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cookiePanicProbe)]
pub fn panic_probe() {
    panic!("the panic probe fired");
}

#[cfg(test)]
mod tests {
    use super::report;

    #[test]
    fn a_report_names_the_page_then_the_panic_and_its_stack() {
        assert_eq!(
            report("panicked at src/a.rs:1:2:\nboom", Some("  at f (x.js:3)\n")),
            "cookie web client panicked at src/a.rs:1:2:\nboom\n\nStack:\nat f (x.js:3)"
        );
    }

    #[test]
    fn a_report_without_a_stack_is_the_panic_alone() {
        let panic = "panicked at src/a.rs:1:2:\nboom";
        assert_eq!(report(panic, None), format!("cookie web client {panic}"));
        assert_eq!(
            report(panic, Some("  \n")),
            format!("cookie web client {panic}")
        );
    }
}
