//! Build script — embed the Autoshop icon into the Windows executables so the
//! .exe file, its desktop shortcut, and the taskbar show our icon instead of the
//! generic Rust binary icon. No-op on non-Windows. The embed is best-effort: if a
//! resource compiler isn't available it warns and the build still succeeds.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/autoshop.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/autoshop.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed skipped (no resource compiler?): {e}");
        }
    }
}
