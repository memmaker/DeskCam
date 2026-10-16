use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User settings, persisted as JSON in ~/Library/Application Support/DeskCam/config.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global hotkey that stops a recording, e.g. "cmd+alt+r", "ctrl+shift+f12".
    pub stop_hotkey: String,
    /// Where finished recordings go. A leading "~/" is expanded.
    pub output_dir: String,
    /// "hevc" (default), "h264" or "prores".
    pub codec: String,
    pub fps: u32,
    /// Countdown length in seconds before recording starts.
    pub countdown: u32,
    pub system_audio: bool,
    pub microphone: bool,
    /// When both audio sources are on, also keep each as its own (disabled) track next to the mix.
    pub separate_tracks: bool,
    pub show_cursor: bool,
    /// AAC bitrate per track, in kbit/s.
    pub audio_kbps: u32,
    /// Remembered capture target ("display:<id>" or "window:<id>").
    pub last_target: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stop_hotkey: "cmd+alt+r".into(),
            output_dir: "~/Movies/DeskCam".into(),
            codec: "hevc".into(),
            fps: 60,
            countdown: 3,
            system_audio: true,
            microphone: true,
            separate_tracks: true,
            show_cursor: true,
            audio_kbps: 320,
            last_target: None,
        }
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

fn config_path() -> PathBuf {
    home().join("Library/Application Support/DeskCam/config.json")
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        let cfg: Config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Write it back so the file exists (with any new keys) for hand-editing.
        cfg.save();
        cfg
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }

    pub fn output_dir(&self) -> PathBuf {
        match self.output_dir.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(&self.output_dir),
        }
    }

    pub fn config_path() -> PathBuf {
        config_path()
    }
}
