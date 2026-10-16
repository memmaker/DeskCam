//! Live capture: ScreenCaptureKit stream -> AVAssetWriter.
//!
//! Everything lands in a fragmented QuickTime file first (video encoded once in
//! its final codec, each audio source as its own lossless LPCM track). A crash
//! mid-recording therefore still leaves a playable file, and the separate
//! sources can be mixed and AAC-encoded afterwards without a second lossy
//! generation (see `finalize`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::*;
use objc2_core_graphics::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSCopying, NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_screen_capture_kit::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
    ProRes,
}

impl Codec {
    pub fn from_name(s: &str) -> Codec {
        match s {
            "h264" => Codec::H264,
            "prores" => Codec::ProRes,
            _ => Codec::Hevc,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
            Codec::ProRes => "prores",
        }
    }
}

pub struct Params {
    /// "display:<id>" or "window:<id>".
    pub target: String,
    pub codec: Codec,
    pub fps: u32,
    pub system_audio: bool,
    pub microphone: bool,
    pub show_cursor: bool,
    /// Intermediate .mov written during capture.
    pub temp_path: PathBuf,
}

pub enum Event {
    Started(Recorder),
    Failed(String),
    /// Capture is over and the intermediate file is closed. `error` is set if
    /// the stream ended on its own (e.g. the recorded window was closed).
    Stopped { error: Option<String> },
}

pub type Notify = fn(Event);

pub struct TargetInfo {
    pub key: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Helpers for building Foundation settings dictionaries.

pub(crate) type Dict = NSDictionary<NSString, AnyObject>;

pub(crate) fn obj<T: objc2::Message>(v: Retained<T>) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(v) }
}
pub(crate) fn num(v: i64) -> Retained<AnyObject> {
    obj(NSNumber::new_i64(v))
}
pub(crate) fn boolean(v: bool) -> Retained<AnyObject> {
    obj(NSNumber::new_bool(v))
}
pub(crate) fn nsstr(s: &str) -> Retained<AnyObject> {
    obj(NSString::from_str(s))
}
pub(crate) fn dict(pairs: &[(&NSString, Retained<AnyObject>)]) -> Retained<Dict> {
    let keys: Vec<&NSString> = pairs.iter().map(|(k, _)| *k).collect();
    let vals: Vec<&AnyObject> = pairs.iter().map(|(_, v)| &**v).collect();
    NSDictionary::from_slices(&keys, &vals)
}
pub(crate) fn key(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}
pub(crate) fn err_text(e: *mut NSError) -> String {
    match unsafe { e.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "unknown error".into(),
    }
}

const LPCM: i64 = 0x6C70_636D; // 'lpcm'
pub(crate) const AAC: i64 = 0x6161_6320; // 'aac '
const PIXEL_420V: u32 = 0x3432_3076; // kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange

/// Float32 interleaved stereo at 48 kHz: lossless home for both sources,
/// whatever format the device delivers.
pub(crate) fn lpcm_settings() -> Retained<Dict> {
    dict(&[
        (&key("AVFormatIDKey"), num(LPCM)),
        (&key("AVSampleRateKey"), num(48_000)),
        (&key("AVNumberOfChannelsKey"), num(2)),
        (&key("AVLinearPCMBitDepthKey"), num(32)),
        (&key("AVLinearPCMIsFloatKey"), boolean(true)),
        (&key("AVLinearPCMIsNonInterleaved"), boolean(false)),
        (&key("AVLinearPCMIsBigEndianKey"), boolean(false)),
    ])
}

fn video_settings(codec: Codec, width: usize, height: usize, fps: u32) -> Retained<Dict> {
    unsafe {
        let codec_type = match codec {
            Codec::Hevc => AVVideoCodecTypeHEVC,
            Codec::H264 => AVVideoCodecTypeH264,
            Codec::ProRes => AVVideoCodecTypeAppleProRes422,
        }
        .unwrap();
        let color = dict(&[
            (AVVideoColorPrimariesKey.unwrap(), obj(AVVideoColorPrimaries_ITU_R_709_2.unwrap().copy())),
            (AVVideoTransferFunctionKey.unwrap(), obj(AVVideoTransferFunction_ITU_R_709_2.unwrap().copy())),
            (AVVideoYCbCrMatrixKey.unwrap(), obj(AVVideoYCbCrMatrix_ITU_R_709_2.unwrap().copy())),
        ]);
        let mut pairs = vec![
            (AVVideoCodecKey.unwrap(), obj(codec_type.copy())),
            (AVVideoWidthKey.unwrap(), num(width as i64)),
            (AVVideoHeightKey.unwrap(), num(height as i64)),
            (AVVideoColorPropertiesKey.unwrap(), obj(color)),
        ];
        if codec != Codec::ProRes {
            // Bits per pixel per frame; ~30 Mbit/s HEVC / ~48 Mbit/s H.264 at 1440p60.
            let bpp = if codec == Codec::Hevc { 0.135 } else { 0.22 };
            let bitrate = ((width * height) as f64 * fps as f64 * bpp).max(8_000_000.0) as i64;
            let profile = if codec == Codec::Hevc { "HEVC_Main_AutoLevel" } else { "H264_High_AutoLevel" };
            let props = dict(&[
                (AVVideoAverageBitRateKey.unwrap(), num(bitrate)),
                (AVVideoExpectedSourceFrameRateKey.unwrap(), num(fps as i64)),
                (AVVideoMaxKeyFrameIntervalKey.unwrap(), num(fps as i64 * 2)),
                (AVVideoProfileLevelKey.unwrap(), nsstr(profile)),
            ]);
            pairs.push((AVVideoCompressionPropertiesKey.unwrap(), obj(props)));
        }
        dict(&pairs)
    }
}

// ---------------------------------------------------------------------------
// Writer state shared between the stream callbacks and stop().

struct Writer {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    system: Option<Retained<AVAssetWriterInput>>,
    mic: Option<Retained<AVAssetWriterInput>>,
    started: bool,
    done: bool,
}

// AVAssetWriter is thread-safe as long as calls are serialized, which the Mutex does.
unsafe impl Send for Writer {}

pub struct Shared {
    w: Mutex<Writer>,
    notify: Notify,
}

impl Shared {
    fn on_sample(&self, sb: &CMSampleBuffer, kind: SCStreamOutputType) {
        if !unsafe { sb.is_valid() } {
            return;
        }
        let mut w = self.w.lock().unwrap();
        if w.done {
            return;
        }
        unsafe {
            if kind == SCStreamOutputType::Screen {
                // Idle/blank status frames carry no image; skip them.
                if sb.image_buffer().is_none() {
                    return;
                }
                if !w.started {
                    w.writer.startSessionAtSourceTime(sb.presentation_time_stamp());
                    w.started = true;
                }
                if w.video.isReadyForMoreMediaData() {
                    w.video.appendSampleBuffer(sb);
                }
            } else {
                // Audio before the first video frame would precede the timeline; drop it.
                if !w.started {
                    return;
                }
                let input = if kind == SCStreamOutputType::Audio { &w.system } else { &w.mic };
                if let Some(input) = input {
                    if input.isReadyForMoreMediaData() {
                        input.appendSampleBuffer(sb);
                    }
                }
            }
        }
    }

    /// Closes the intermediate file. Must run on the sample queue so it is
    /// ordered after every pending sample callback.
    fn finish(&self, error: Option<String>) {
        let mut w = self.w.lock().unwrap();
        if w.done {
            return;
        }
        w.done = true;
        let notify = self.notify;
        unsafe {
            if !w.started {
                w.writer.cancelWriting();
                notify(Event::Stopped { error: Some(error.unwrap_or_else(|| "no video frames were captured".into())) });
                return;
            }
            w.video.markAsFinished();
            for input in [&w.system, &w.mic].into_iter().flatten() {
                input.markAsFinished();
            }
            let writer = w.writer.clone();
            let block = RcBlock::new(move || {
                let error = match writer.status() {
                    AVAssetWriterStatus::Completed => error.clone(),
                    _ => Some(writer.error().map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "writing failed".into())),
                };
                notify(Event::Stopped { error });
            });
            w.writer.finishWritingWithCompletionHandler(&block);
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DeskCamStreamOutput"]
    #[ivars = Arc<Shared>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sb: &CMSampleBuffer, kind: SCStreamOutputType) {
            self.ivars().on_sample(sb, kind);
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            // Called on an arbitrary queue; the writer mutex keeps it safe.
            self.ivars().finish(Some(error.localizedDescription().to_string()));
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------

pub struct Recorder {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    shared: Arc<Shared>,
    queue: DispatchRetained<DispatchQueue>,
}

unsafe impl Send for Recorder {}

impl Recorder {
    pub fn stop(&self) {
        let shared = self.shared.clone();
        let queue = self.queue.clone();
        let block = RcBlock::new(move |_err: *mut NSError| {
            let shared = shared.clone();
            queue.exec_async(move || shared.finish(None));
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
    }
}

/// Lists capturable displays and windows. `cb` runs on a background queue.
pub fn list_targets(cb: fn(Result<Vec<TargetInfo>, String>)) {
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let Some(content) = (unsafe { content.as_ref() }) else {
            cb(Err(err_text(err)));
            return;
        };
        let me = std::process::id() as i32;
        let mut out = Vec::new();
        unsafe {
            for (i, d) in content.displays().iter().enumerate() {
                out.push(TargetInfo {
                    key: format!("display:{}", d.displayID()),
                    label: format!("Display {} — {}×{}", i + 1, d.width(), d.height()),
                });
            }
            let mut windows: Vec<TargetInfo> = content
                .windows()
                .iter()
                .filter(|w| w.windowLayer() == 0 && w.frame().size.width >= 64.0 && w.frame().size.height >= 64.0)
                .filter_map(|w| {
                    let app = w.owningApplication()?;
                    if app.processID() == me {
                        return None;
                    }
                    let app_name = app.applicationName().to_string();
                    let title = w.title().map(|t| t.to_string()).unwrap_or_default();
                    if app_name.is_empty() && title.is_empty() {
                        return None;
                    }
                    let label = if title.is_empty() { app_name } else { format!("{app_name} — {title}") };
                    Some(TargetInfo { key: format!("window:{}", w.windowID()), label })
                })
                .collect();
            windows.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
            out.extend(windows);
        }
        cb(Ok(out));
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        )
    };
}

/// Starts capturing. Exactly one of `Event::Started` / `Event::Failed` is
/// delivered through `notify` (from a background queue).
pub fn start(params: Params, notify: Notify) {
    let params = Arc::new(Mutex::new(Some(params)));
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let Some(params) = params.lock().unwrap().take() else { return };
        let Some(content) = (unsafe { content.as_ref() }) else {
            notify(Event::Failed(format!("Screen recording not permitted: {}", err_text(err))));
            return;
        };
        if let Err(e) = unsafe { build_and_start(content, params, notify) } {
            notify(Event::Failed(e));
        }
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &block,
        )
    };
}

unsafe fn build_and_start(content: &SCShareableContent, p: Params, notify: Notify) -> Result<(), String> {
    unsafe {
        let (kind, id) = p.target.split_once(':').ok_or("bad target")?;
        let id: u32 = id.parse().map_err(|_| "bad target id")?;

        let filter = match kind {
            "display" => {
                let display = content
                    .displays()
                    .iter()
                    .find(|d| d.displayID() == id)
                    .ok_or("That display is no longer connected.")?;
                // Keep our own widget/countdown out of the picture.
                let me = std::process::id() as i32;
                let ours: Vec<_> = content.applications().iter().filter(|a| a.processID() == me).collect();
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::from_retained_slice(&ours),
                    &NSArray::new(),
                )
            }
            _ => {
                let window = content
                    .windows()
                    .iter()
                    .find(|w| w.windowID() == id)
                    .ok_or("That window is gone — pick it again.")?;
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            }
        };

        let rect = filter.contentRect();
        let scale = filter.pointPixelScale() as f64;
        let even = |v: f64| ((v * scale).round() as usize).max(2) & !1;
        let (width, height) = (even(rect.size.width), even(rect.size.height));

        let config = SCStreamConfiguration::new();
        config.setWidth(width);
        config.setHeight(height);
        config.setMinimumFrameInterval(CMTime::new(1, p.fps as i32));
        config.setPixelFormat(PIXEL_420V);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setShowsCursor(p.show_cursor);
        config.setQueueDepth(8);
        if p.system_audio {
            config.setCapturesAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
        }
        if p.microphone {
            config.setCaptureMicrophone(true);
        }

        // Intermediate writer.
        let _ = std::fs::remove_file(&p.temp_path);
        let url = NSURL::fileURLWithPath(&NSString::from_str(&p.temp_path.to_string_lossy()));
        let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url, AVFileTypeQuickTimeMovie.unwrap())
            .map_err(|e| format!("Cannot create file: {}", e.localizedDescription()))?;
        writer.setMovieFragmentInterval(CMTime::new(5, 1));

        let video = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            AVMediaTypeVideo.unwrap(),
            Some(&video_settings(p.codec, width, height, p.fps)),
        );
        video.setExpectsMediaDataInRealTime(true);
        if !writer.canAddInput(&video) {
            return Err(format!("{} encoding is not supported at {width}×{height}", p.codec.name()));
        }
        writer.addInput(&video);

        let audio_input = |enabled: bool| -> Option<Retained<AVAssetWriterInput>> {
            if !enabled {
                return None;
            }
            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                AVMediaTypeAudio.unwrap(),
                Some(&lpcm_settings()),
            );
            input.setExpectsMediaDataInRealTime(true);
            writer.addInput(&input);
            Some(input)
        };
        // Track order in the intermediate file: video, system, mic.
        let system = audio_input(p.system_audio);
        let mic = audio_input(p.microphone);

        if !writer.startWriting() {
            return Err(format!(
                "Cannot start writer: {}",
                writer.error().map(|e| e.localizedDescription().to_string()).unwrap_or_default()
            ));
        }

        let shared = Arc::new(Shared {
            w: Mutex::new(Writer { writer, video, system, mic, started: false, done: false }),
            notify,
        });
        let output = StreamOutput::new(shared.clone());
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        );
        let queue = DispatchQueue::new("com.deskcam.samples", None);
        let out_proto = ProtocolObject::from_ref(&*output);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Screen, Some(&queue))
            .map_err(|e| e.localizedDescription().to_string())?;
        if p.system_audio {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Audio, Some(&queue))
                .map_err(|e| e.localizedDescription().to_string())?;
        }
        if p.microphone {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Microphone, Some(&queue))
                .map_err(|e| e.localizedDescription().to_string())?;
        }

        let recorder = Arc::new(Mutex::new(Some(Recorder { stream: stream.clone(), _output: output, shared, queue })));
        let block = RcBlock::new(move |err: *mut NSError| {
            let Some(rec) = recorder.lock().unwrap().take() else { return };
            if err.is_null() {
                notify(Event::Started(rec));
            } else {
                rec.shared.w.lock().unwrap().writer.cancelWriting();
                notify(Event::Failed(format!("Capture failed to start: {}", err_text(err))));
            }
        });
        stream.startCaptureWithCompletionHandler(Some(&block));
        Ok(())
    }
}
