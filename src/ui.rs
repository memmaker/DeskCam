//! The floating widget, the countdown overlay and the recording state machine.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::PathBuf;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSDate, NSDateFormatter, NSNotification, NSObject, NSObjectProtocol, NSString, NSTimer, NSURL};

use crate::config::Config;
use crate::finalize;
use crate::hotkey::{self, HotKey};
use crate::recorder::{self, Codec, Event, Recorder, TargetInfo};

const CODECS: [(&str, &str); 3] = [
    ("hevc", "HEVC (H.265) · MP4"),
    ("h264", "H.264 · MP4"),
    ("prores", "ProRes 422 · MOV"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Idle,
    Countdown,
    Starting,
    Recording,
    Finishing,
}

/// Messages from background threads, delivered on the main thread.
enum Msg {
    Rec(Event),
    Targets(Result<Vec<TargetInfo>, String>),
    Finalized(Result<PathBuf, String>),
}

fn post(msg: Msg) {
    DispatchQueue::main().exec_async(move || {
        CONTROLLER.with(|c| {
            if let Some(c) = c.get() {
                c.handle(msg);
            }
        })
    });
}

thread_local! {
    static CONTROLLER: OnceCell<Retained<Controller>> = const { OnceCell::new() };
}

fn on_hotkey() {
    CONTROLLER.with(|c| {
        if let Some(c) = c.get() {
            c.stop_recording();
        }
    });
}

#[derive(Default)]
pub struct Ivars {
    cfg: RefCell<Config>,
    state: Cell<State>,
    panel: OnceCell<Retained<NSPanel>>,
    record_btn: OnceCell<Retained<NSButton>>,
    target_popup: OnceCell<Retained<NSPopUpButton>>,
    system_cb: OnceCell<Retained<NSButton>>,
    mic_cb: OnceCell<Retained<NSButton>>,
    codec_popup: OnceCell<Retained<NSPopUpButton>>,
    status: OnceCell<Retained<NSTextField>>,
    overlay: OnceCell<Retained<NSWindow>>,
    overlay_label: OnceCell<Retained<NSTextField>>,
    targets: RefCell<Vec<TargetInfo>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    remaining: Cell<u32>,
    recorder: RefCell<Option<Recorder>>,
    hotkey: RefCell<Option<HotKey>>,
    /// (intermediate .mov, final file) of the current recording.
    paths: RefCell<Option<(PathBuf, PathBuf)>>,
    stop_error: RefCell<Option<String>>,
}

impl Default for State {
    fn default() -> Self {
        State::Idle
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DeskCamController"]
    #[ivars = Ivars]
    pub struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            if let Some(rec) = self.ivars().recorder.borrow().as_ref() {
                rec.stop();
            }
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }

    impl Controller {
        #[unsafe(method(record:))]
        fn record_clicked(&self, _sender: Option<&AnyObject>) {
            match self.ivars().state.get() {
                State::Idle => self.begin_countdown(),
                State::Countdown => self.cancel_countdown(),
                _ => {}
            }
        }

        #[unsafe(method(refresh:))]
        fn refresh_clicked(&self, _sender: Option<&AnyObject>) {
            self.refresh_targets();
        }

        #[unsafe(method(settingsChanged:))]
        fn settings_changed(&self, _sender: Option<&AnyObject>) {
            self.read_settings();
            if self.ivars().cfg.borrow().microphone {
                request_mic_access();
            }
        }
    }
);

fn on(b: &NSButton) -> bool {
    b.state() == NSControlStateValueOn
}

fn request_mic_access() {
    unsafe {
        let block = RcBlock::new(|_granted: objc2::runtime::Bool| {});
        objc2_av_foundation::AVCaptureDevice::requestAccessForMediaType_completionHandler(
            objc2_av_foundation::AVMediaTypeAudio.unwrap(),
            &block,
        );
    }
}

impl Controller {
    pub fn install(mtm: MainThreadMarker) {
        let this = mtm.alloc::<Self>().set_ivars(Ivars { cfg: RefCell::new(Config::load()), ..Default::default() });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.build_widget(mtm);
        this.build_overlay(mtm);
        hotkey::install(on_hotkey);
        if this.ivars().cfg.borrow().microphone {
            request_mic_access();
        }
        this.refresh_targets();
        CONTROLLER.with(|c| {
            let _ = c.set(this);
        });
    }

    fn build_widget(&self, mtm: MainThreadMarker) {
        unsafe { self.build_widget_inner(mtm) }
    }

    unsafe fn build_widget_inner(&self, mtm: MainThreadMarker) {
        unsafe {
        let iv = self.ivars();
        let cfg = iv.cfg.borrow().clone();
        let (w, h) = (320.0, 226.0);
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::UtilityWindow;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect(0.0, 0.0, w, h),
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setReleasedWhenClosed(false);
        panel.setTitle(&NSString::from_str("DeskCam"));
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = panel.contentView().unwrap();
        let target: &AnyObject = self.as_ref();

        let record = NSButton::buttonWithTitle_target_action(&NSString::from_str("●  Record"), Some(target), Some(sel!(record:)), mtm);
        record.setFrame(rect(16.0, h - 64.0, w - 32.0, 48.0));
        record.setBezelStyle(NSBezelStyle::Push);
        record.setControlSize(NSControlSize::Large);
        record.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
        record.setBezelColor(Some(&NSColor::systemRedColor()));
        record.setKeyEquivalent(&NSString::from_str("\r"));
        content.addSubview(&record);

        let popup = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), rect(16.0, h - 100.0, w - 64.0, 28.0), false);
        popup.setTarget(Some(target));
        popup.setAction(Some(sel!(settingsChanged:)));
        content.addSubview(&popup);

        let refresh = NSButton::buttonWithTitle_target_action(&NSString::from_str("↻"), Some(target), Some(sel!(refresh:)), mtm);
        refresh.setFrame(rect(w - 46.0, h - 100.0, 32.0, 28.0));
        refresh.setToolTip(Some(&NSString::from_str("Refresh window list")));
        content.addSubview(&refresh);

        let check = |title: &str, x: f64, value: bool| {
            let cb = NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), Some(target), Some(sel!(settingsChanged:)), mtm);
            cb.setFrame(rect(x, h - 130.0, 140.0, 22.0));
            cb.setState(if value { NSControlStateValueOn } else { NSControlStateValueOff });
            content.addSubview(&cb);
            cb
        };
        let system_cb = check("Computer audio", 18.0, cfg.system_audio);
        let mic_cb = check("Microphone", 170.0, cfg.microphone);

        let codec = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), rect(16.0, h - 164.0, w - 32.0, 28.0), false);
        for (i, (name, label)) in CODECS.iter().enumerate() {
            codec.addItemWithTitle(&NSString::from_str(label));
            if *name == cfg.codec {
                codec.selectItemAtIndex(i as isize);
            }
        }
        codec.setTarget(Some(target));
        codec.setAction(Some(sel!(settingsChanged:)));
        content.addSubview(&codec);

        let hint = NSTextField::labelWithString(
            &NSString::from_str(&format!("Stop recording with {}", hotkey::display(&cfg.stop_hotkey))),
            mtm,
        );
        hint.setFrame(rect(18.0, h - 190.0, w - 36.0, 18.0));
        hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
        hint.setToolTip(Some(&NSString::from_str(&format!("Change in {}", Config::config_path().display()))));
        content.addSubview(&hint);

        let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        status.setFrame(rect(18.0, 8.0, w - 36.0, 18.0));
        status.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        status.setTextColor(Some(&NSColor::secondaryLabelColor()));
        status.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        content.addSubview(&status);

        // Default spot: top-right corner; afterwards wherever the user dragged it.
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let vf = screen.visibleFrame();
            panel.setFrameTopLeftPoint(CGPoint::new(vf.origin.x + vf.size.width - w - 24.0, vf.origin.y + vf.size.height - 24.0));
        }
        panel.setFrameAutosaveName(&NSString::from_str("DeskCamWidget"));

        let _ = iv.record_btn.set(record);
        let _ = iv.target_popup.set(popup);
        let _ = iv.system_cb.set(system_cb);
        let _ = iv.mic_cb.set(mic_cb);
        let _ = iv.codec_popup.set(codec);
        let _ = iv.status.set(status);
        let _ = iv.panel.set(panel);
        self.show_widget();
        }
    }

    fn build_overlay(&self, mtm: MainThreadMarker) {
        let size = 200.0;
        let win = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(0.0, 0.0, size, size),
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { win.setReleasedWhenClosed(false) };
        win.setOpaque(false);
        win.setBackgroundColor(Some(&NSColor::clearColor()));
        win.setIgnoresMouseEvents(true);
        win.setLevel(NSScreenSaverWindowLevel);
        win.setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary);

        let bg = NSBox::initWithFrame(mtm.alloc(), rect(0.0, 0.0, size, size));
        bg.setBoxType(NSBoxType::Custom);
        bg.setBorderWidth(0.0);
        bg.setCornerRadius(size / 2.0);
        bg.setFillColor(&NSColor::colorWithWhite_alpha(0.0, 0.72));
        bg.setTitlePosition(NSTitlePosition::NoTitle);

        let label = NSTextField::labelWithString(&NSString::from_str("3"), mtm);
        label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(110.0, unsafe { NSFontWeightBold })));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setAlignment(NSTextAlignment::Center);
        label.setFrame(rect(0.0, (size - 132.0) / 2.0, size, 132.0));

        let content = win.contentView().unwrap();
        content.addSubview(&bg);
        content.addSubview(&label);
        let _ = self.ivars().overlay.set(win);
        let _ = self.ivars().overlay_label.set(label);
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::from(self)
    }

    fn set_status(&self, s: &str) {
        self.ivars().status.get().unwrap().setStringValue(&NSString::from_str(s));
    }

    fn set_state(&self, st: State) {
        let iv = self.ivars();
        iv.state.set(st);
        let btn = iv.record_btn.get().unwrap();
        let (title, enabled) = match st {
            State::Idle => ("●  Record", true),
            State::Countdown => ("Cancel", true),
            State::Starting | State::Recording => ("Recording…", false),
            State::Finishing => ("Saving…", false),
        };
        btn.setTitle(&NSString::from_str(title));
        btn.setEnabled(enabled);
        let editable = st == State::Idle;
        for c in [iv.system_cb.get(), iv.mic_cb.get()].into_iter().flatten() {
            c.setEnabled(editable);
        }
        iv.target_popup.get().unwrap().setEnabled(editable);
        iv.codec_popup.get().unwrap().setEnabled(editable);
    }

    fn show_widget(&self) {
        let app = NSApplication::sharedApplication(self.mtm());
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        self.ivars().panel.get().unwrap().makeKeyAndOrderFront(None);
    }

    fn hide_widget(&self) {
        self.ivars().panel.get().unwrap().orderOut(None);
    }

    fn read_settings(&self) {
        let iv = self.ivars();
        let mut cfg = iv.cfg.borrow_mut();
        cfg.system_audio = on(iv.system_cb.get().unwrap());
        cfg.microphone = on(iv.mic_cb.get().unwrap());
        let ci = iv.codec_popup.get().unwrap().indexOfSelectedItem();
        cfg.codec = CODECS.get(ci as usize).map(|c| c.0).unwrap_or("hevc").into();
        let ti = iv.target_popup.get().unwrap().indexOfSelectedItem();
        if let Some(t) = iv.targets.borrow().get(ti as usize) {
            cfg.last_target = Some(t.key.clone());
        }
        cfg.save();
    }

    fn refresh_targets(&self) {
        recorder::list_targets(|r| post(Msg::Targets(r)));
    }

    fn set_targets(&self, targets: Vec<TargetInfo>) {
        let iv = self.ivars();
        let popup = iv.target_popup.get().unwrap();
        popup.removeAllItems();
        let want = iv.cfg.borrow().last_target.clone();
        let mut sel = 0;
        for (i, t) in targets.iter().enumerate() {
            // addItemWithTitle dedups identical titles; disambiguate first.
            let mut title = t.label.clone();
            while popup.itemWithTitle(&NSString::from_str(&title)).is_some() {
                title.push(' ');
            }
            popup.addItemWithTitle(&NSString::from_str(&title));
            if Some(&t.key) == want.as_ref() {
                sel = i;
            }
            // Separator-ish: label displays vs windows visually via the menu.
        }
        if !targets.is_empty() {
            popup.selectItemAtIndex(sel as isize);
        }
        *iv.targets.borrow_mut() = targets;
    }

    // --- Countdown -------------------------------------------------------

    fn begin_countdown(&self) {
        self.read_settings();
        let iv = self.ivars();
        if iv.targets.borrow().is_empty() {
            self.set_status("Nothing to record — grant Screen Recording permission and press ↻");
            return;
        }
        let n = iv.cfg.borrow().countdown;
        if n == 0 {
            self.hide_widget();
            self.start_recording();
            return;
        }
        self.set_state(State::Countdown);
        self.set_status("");
        iv.remaining.set(n);
        self.show_overlay(n);

        let block = RcBlock::new(|_t: std::ptr::NonNull<NSTimer>| {
            CONTROLLER.with(|c| {
                if let Some(c) = c.get() {
                    c.tick();
                }
            })
        });
        let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) };
        *iv.timer.borrow_mut() = Some(timer);
    }

    fn tick(&self) {
        let iv = self.ivars();
        let n = iv.remaining.get().saturating_sub(1);
        iv.remaining.set(n);
        if n > 0 {
            self.show_overlay(n);
            return;
        }
        self.stop_timer();
        iv.overlay.get().unwrap().orderOut(None);
        self.hide_widget();
        self.start_recording();
    }

    fn cancel_countdown(&self) {
        self.stop_timer();
        self.ivars().overlay.get().unwrap().orderOut(None);
        self.set_state(State::Idle);
    }

    fn stop_timer(&self) {
        if let Some(t) = self.ivars().timer.borrow_mut().take() {
            t.invalidate();
        }
    }

    fn show_overlay(&self, n: u32) {
        let iv = self.ivars();
        let win = iv.overlay.get().unwrap();
        iv.overlay_label.get().unwrap().setStringValue(&NSString::from_str(&n.to_string()));
        // Center on the screen the widget lives on.
        let screen = iv.panel.get().unwrap().screen().or_else(|| NSScreen::mainScreen(self.mtm()));
        if let Some(screen) = screen {
            let f = screen.frame();
            let s = win.frame().size;
            win.setFrameOrigin(CGPoint::new(
                f.origin.x + (f.size.width - s.width) / 2.0,
                f.origin.y + (f.size.height - s.height) / 2.0,
            ));
        }
        win.orderFrontRegardless();
    }

    // --- Recording -------------------------------------------------------

    fn start_recording(&self) {
        let iv = self.ivars();
        let cfg = iv.cfg.borrow().clone();
        let dir = cfg.output_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return self.fail(&format!("Cannot create {}: {e}", dir.display()));
        }
        let codec = Codec::from_name(&cfg.codec);
        let stamp = timestamp();
        let ext = if codec == Codec::ProRes { "mov" } else { "mp4" };
        let out = dir.join(format!("DeskCam {stamp}.{ext}"));
        let temp = dir.join(format!(".DeskCam {stamp}.capture.mov"));
        *iv.paths.borrow_mut() = Some((temp.clone(), out));
        *iv.stop_error.borrow_mut() = None;

        match HotKey::register(&cfg.stop_hotkey) {
            Ok(h) => *iv.hotkey.borrow_mut() = Some(h),
            Err(e) => return self.fail(&format!("{e} — refusing to record without a way to stop")),
        }

        self.set_state(State::Starting);
        let Some(target) = cfg.last_target.clone() else {
            return self.fail("Pick something to record");
        };
        recorder::start(
            recorder::Params {
                target,
                codec,
                fps: cfg.fps.clamp(1, 240),
                system_audio: cfg.system_audio,
                microphone: cfg.microphone,
                show_cursor: cfg.show_cursor,
                temp_path: temp,
            },
            |ev| post(Msg::Rec(ev)),
        );
    }

    fn fail(&self, msg: &str) {
        self.ivars().hotkey.borrow_mut().take();
        self.set_state(State::Idle);
        self.set_status(msg);
        self.show_widget();
        NSBeep();
    }

    fn stop_recording(&self) {
        let iv = self.ivars();
        if iv.state.get() != State::Recording {
            return;
        }
        iv.hotkey.borrow_mut().take();
        self.set_state(State::Finishing);
        self.set_status("Saving…");
        self.show_widget();
        if let Some(rec) = iv.recorder.borrow().as_ref() {
            rec.stop();
        }
    }

    fn handle(&self, msg: Msg) {
        let iv = self.ivars();
        match msg {
            Msg::Targets(Ok(t)) => self.set_targets(t),
            Msg::Targets(Err(e)) => self.set_status(&format!("Screen Recording permission needed: {e}")),
            Msg::Rec(Event::Started(rec)) => {
                *iv.recorder.borrow_mut() = Some(rec);
                self.set_state(State::Recording);
            }
            Msg::Rec(Event::Failed(e)) => {
                *iv.paths.borrow_mut() = None;
                self.fail(&e);
            }
            Msg::Rec(Event::Stopped { error }) => {
                // Either our stop() finished, or the stream died on its own
                // (window closed, display unplugged) — in that case surface it.
                iv.hotkey.borrow_mut().take();
                iv.recorder.borrow_mut().take();
                self.set_state(State::Finishing);
                self.show_widget();
                *iv.stop_error.borrow_mut() = error.clone();
                let Some((temp, out)) = iv.paths.borrow_mut().take() else { return };
                if !temp.exists() {
                    return self.fail(&error.unwrap_or_else(|| "Recording failed".into()));
                }
                self.set_status("Saving…");
                let cfg = iv.cfg.borrow().clone();
                let mp4 = out.extension().is_some_and(|e| e == "mp4");
                std::thread::spawn(move || {
                    let r = finalize::run(&temp, &out, mp4, cfg.audio_kbps, cfg.separate_tracks);
                    let r = match r {
                        Ok(()) => {
                            let _ = std::fs::remove_file(&temp);
                            Ok(out)
                        }
                        Err(e) => {
                            // Keep the raw capture visible so nothing is lost.
                            let _ = std::fs::remove_file(&out);
                            let keep = out.with_extension("raw.mov");
                            let _ = std::fs::rename(&temp, &keep);
                            Err(format!("Mixdown failed ({e}); raw capture kept as {}", keep.display()))
                        }
                    };
                    post(Msg::Finalized(r));
                });
            }
            Msg::Finalized(r) => {
                self.set_state(State::Idle);
                let note = iv.stop_error.borrow_mut().take();
                match r {
                    Ok(path) => {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        match note {
                            Some(n) => self.set_status(&format!("Saved {name} (stopped early: {n})")),
                            None => self.set_status(&format!("Saved {name}")),
                        }
                        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                        NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
                    }
                    Err(e) => {
                        self.set_status(&e);
                        NSBeep();
                    }
                }
                self.refresh_targets();
            }
        }
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x as CGFloat, y as CGFloat), CGSize::new(w as CGFloat, h as CGFloat))
}

fn timestamp() -> String {
    let f = NSDateFormatter::new();
    f.setDateFormat(Some(&NSString::from_str("yyyy-MM-dd HH.mm.ss")));
    f.stringFromDate(&NSDate::now()).to_string()
}
