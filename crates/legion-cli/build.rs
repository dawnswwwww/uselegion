//! Build script that captures the compilation target triple so the CLI can
//! match downloaded Gateway artifacts to the platform it is running on.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=LEGION_TARGET={target}");
}
