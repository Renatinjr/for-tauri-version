// No console window behind the video on Windows. Debug builds keep it, because that is
// where the log goes while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    signage_desktop::run()
}
