// Prevents an extra console window from appearing on Windows release
// builds; no-op on macOS and Linux.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    mangoprint_prepress_pdf_lib::run()
}
