mod config;
mod finalize;
mod hotkey;
mod recorder;
mod ui;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    // Debug aid: `deskcam --remux CAPTURE.mov OUT.mp4` runs only the mixdown step.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "--remux" {
        let out = std::path::Path::new(&args[3]);
        let mp4 = out.extension().is_some_and(|e| e == "mp4");
        match finalize::run(std::path::Path::new(&args[2]), out, mp4, 320, true) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("remux failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Widget-only app: no Dock icon (the bundle also sets LSUIElement).
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    ui::Controller::install(mtm);
    app.run();
}
