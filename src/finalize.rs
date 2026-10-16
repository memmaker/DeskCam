//! Turns the intermediate capture into the upload-ready file: video is copied
//! untouched, audio is (mixed and) encoded to AAC exactly once.
//!
//! Output audio tracks:
//!   no source  -> none (muted video)
//!   one source -> that source
//!   both       -> mix (enabled), then mic and system as disabled extra tracks
//!                 if `separate_tracks` is on. YouTube only plays the first.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_av_foundation::*;
use objc2_core_media::CMFormatDescription;
use objc2_foundation::{NSString, NSURL};

use crate::recorder::{dict, key, lpcm_settings, num, AAC};

struct Pair {
    output: Retained<AVAssetReaderOutput>,
    input: Retained<AVAssetWriterInput>,
}

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

fn aac_settings(kbps: u32) -> Retained<crate::recorder::Dict> {
    dict(&[
        (&key("AVFormatIDKey"), num(AAC)),
        (&key("AVSampleRateKey"), num(48_000)),
        (&key("AVNumberOfChannelsKey"), num(2)),
        (&key("AVEncoderBitRateKey"), num(kbps as i64 * 1000)),
    ])
}

pub fn run(temp: &Path, out: &Path, mp4: bool, audio_kbps: u32, separate_tracks: bool) -> Result<(), String> {
    unsafe {
        let url = |p: &Path| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
        let asset = AVURLAsset::URLAssetWithURL_options(&url(temp), None);
        #[allow(deprecated)]
        let videos = asset.tracksWithMediaType(AVMediaTypeVideo.unwrap());
        #[allow(deprecated)]
        let audios = asset.tracksWithMediaType(AVMediaTypeAudio.unwrap());
        let vtrack = videos.firstObject().ok_or("recording contains no video")?;

        let reader = AVAssetReader::assetReaderWithAsset_error(&asset).map_err(|e| e.localizedDescription().to_string())?;
        let _ = std::fs::remove_file(out);
        let file_type = if mp4 { AVFileTypeMPEG4 } else { AVFileTypeQuickTimeMovie }.unwrap();
        let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url(out), file_type)
            .map_err(|e| e.localizedDescription().to_string())?;

        let mut pairs = Vec::new();

        // Video: passthrough. MP4 needs the format hint to write the sample description.
        let fmt = vtrack.formatDescriptions().firstObject().ok_or("video has no format")?;
        let fmt: &CMFormatDescription = &*(Retained::as_ptr(&fmt) as *const CMFormatDescription);
        let vout = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&vtrack, None);
        let vin = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings_sourceFormatHint(
            AVMediaTypeVideo.unwrap(),
            None,
            Some(fmt),
        );
        vin.setTransform(vtrack.preferredTransform());
        pairs.push((Retained::into_super(vout), vin));

        let audio_in = |enabled: bool| {
            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                AVMediaTypeAudio.unwrap(),
                Some(&aac_settings(audio_kbps)),
            );
            input.setMarksOutputTrackAsEnabled(enabled);
            input.setLanguageCode(Some(&NSString::from_str("und")));
            input
        };
        let track_out = |t: &AVAssetTrack| -> Retained<AVAssetReaderOutput> {
            Retained::into_super(AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
                t,
                Some(&lpcm_settings()),
            ))
        };

        match audios.count() {
            0 => {}
            1 => pairs.push((track_out(&audios.objectAtIndex(0)), audio_in(true))),
            _ => {
                let mix = AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(
                    &audios,
                    Some(&lpcm_settings()),
                );
                pairs.push((Retained::into_super(mix), audio_in(true)));
                if separate_tracks {
                    // Intermediate order is system, mic; put the voice first.
                    for i in (0..audios.count()).rev() {
                        pairs.push((track_out(&audios.objectAtIndex(i)), audio_in(false)));
                    }
                }
            }
        }

        for (output, input) in &pairs {
            output.setAlwaysCopiesSampleData(false);
            if !reader.canAddOutput(output) || !writer.canAddInput(input) {
                return Err("cannot set up remux".into());
            }
            reader.addOutput(output);
            writer.addInput(input);
        }

        if !reader.startReading() {
            return Err(format!("read failed: {:?}", reader.error().map(|e| e.localizedDescription().to_string())));
        }
        if !writer.startWriting() {
            return Err(format!("write failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string())));
        }
        writer.startSessionAtSourceTime(objc2_core_media::kCMTimeZero);

        // One pump thread per track: the writer interleaves tracks, so feeding
        // them one after another would stall.
        let threads: Vec<_> = pairs
            .into_iter()
            .map(|(output, input)| {
                let p = SendPtr((Pair { output, input }, writer.clone()));
                std::thread::spawn(move || {
                    let p = p;
                    let (Pair { output, input }, writer) = &p.0;
                    loop {
                        while !input.isReadyForMoreMediaData() {
                            if writer.status() != AVAssetWriterStatus::Writing {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        match output.copyNextSampleBuffer() {
                            Some(sb) => {
                                if !input.appendSampleBuffer(&sb) {
                                    return;
                                }
                            }
                            None => {
                                input.markAsFinished();
                                return;
                            }
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }

        if reader.status() == AVAssetReaderStatus::Failed || writer.status() == AVAssetWriterStatus::Failed {
            let e = reader.error().or_else(|| writer.error());
            writer.cancelWriting();
            return Err(e.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "remux failed".into()));
        }

        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move || {
            let _ = tx.send(());
        });
        writer.finishWritingWithCompletionHandler(&block);
        let _ = rx.recv();
        if writer.status() != AVAssetWriterStatus::Completed {
            return Err(writer.error().map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "finish failed".into()));
        }
        Ok(())
    }
}
