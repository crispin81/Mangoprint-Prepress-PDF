// Prevents an extra console window from appearing on Windows release
// builds; no-op on macOS and Linux.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Same Linux workaround as RapidCulling. Some wlroots-based Wayland
/// compositors (seen on Hyprland) hit a fatal `Error 71 (Protocol error)
/// dispatching to Wayland display` in GTK's native Wayland backend, and
/// separately a `Failed to create GBM buffer` from WebKitGTK's DMA-BUF
/// renderer that leaves the window blank even when it doesn't crash. Both
/// are worked around by falling back to XWayland and software compositing.
/// Set *before* Tauri/GTK initializes, and never overrides a value the user
/// (or the AppImage's GTK hook, which prefers native Wayland) already set.
#[cfg(target_os = "linux")]
fn apply_linux_webkit_workarounds() {
    for (key, value) in [
        ("GDK_BACKEND", "x11,wayland"),
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
        ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
    ] {
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    apply_linux_webkit_workarounds();

    mangoprint_prepress_pdf_lib::run()
}
