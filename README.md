# DeskCam

Minimal macOS screen recorder for YouTube. Pick a display or window, tick the audio sources, press **Record**: 3‑2‑1 countdown, the widget hides, recording runs until the stop hotkey (default ⌥⌘R).

Requires macOS 15+ (system audio and microphone are both captured through ScreenCaptureKit).

## Build

```sh
./bundle.sh          # -> DeskCam.app (ad-hoc signed)
open DeskCam.app
```

With ad-hoc signing macOS asks for Screen Recording permission again after every rebuild. Set `DESKCAM_SIGN="<your certificate name>"` to keep the permission.

## Output

`~/Movies/DeskCam/DeskCam YYYY-MM-DD HH.mm.ss.mp4`

| Setting | Container | Notes |
|---|---|---|
| HEVC (default) | MP4 | about 30 Mbit/s at 1440p60, hardware encoder |
| H.264 | MP4 | about 48 Mbit/s at 1440p60, works with every editor |
| ProRes 422 | MOV | for editing; very large files |

Audio is AAC‑LC at 48 kHz, stereo, 320 kbit/s. When both sources are on, track 1 is the mix (the one YouTube plays), and tracks 2 and 3 are the mic and computer audio on their own, marked disabled.

How it works: the capture goes into a hidden, fragmented `.capture.mov` (video in its final codec, each audio source as lossless PCM). When you stop, that file is remuxed into the final file: video is copied without re-encoding, and audio is mixed and encoded once. If the app crashes, the `.capture.mov` is still playable. If the remux fails, the capture is kept as `*.raw.mov`.

## Config

`~/Library/Application Support/DeskCam/config.json` holds `stop_hotkey` (e.g. `"ctrl+shift+f12"`), `output_dir`, `fps`, `countdown`, `audio_kbps`, `separate_tracks` and `show_cursor`. Restart the app after editing it.

`deskcam --remux CAPTURE.mov OUT.mp4` runs only the mixdown step (for debugging).
