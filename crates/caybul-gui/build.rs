fn main() {
    // Embed the app icon into the Windows .exe so File Explorer / taskbar
    // show it. No-op on other platforms.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/caybul.ico");
        let _ = res.compile();
    }
    // When cross-compiling to Windows from another OS, winresource still runs
    // (build script host target is checked via CARGO_CFG_TARGET_OS).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && !cfg!(windows)
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/caybul.ico");
        // mingw toolchain provides the resource compiler.
        res.set_windres_path("x86_64-w64-mingw32-windres");
        let _ = res.compile();
    }
}
