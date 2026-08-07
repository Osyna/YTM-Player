//! User-tunable settings, edited live in the `s` menu and persisted as plain `key=value`
//! lines. Persistence matters most for smart loading: it decides how the *next* launch
//! resolves a playlist, so a session-only flag would be pointless.
//!
//! ponytail: hand-rolled kv parsing; a config-format dependency only when settings outgrow
//! three keys.

use std::path::PathBuf;

/// Crossfade length bounds, seconds. The value is the whole transition: half fades the
/// outgoing track, half the incoming one.
pub const CROSSFADE_MIN: u16 = 3;
pub const CROSSFADE_MAX: u16 = 15;

/// What `d` saves for a source that has a picture at all. Audio-only sources (SoundCloud,
/// Spotify) always save MP3 no matter what this says - see [`crate::youtube::DownloadSpec`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SaveFormat {
    #[default]
    Mp3,
    Mp4,
}

impl SaveFormat {
    pub fn label(self) -> &'static str {
        match self {
            SaveFormat::Mp3 => "MP3 (audio)",
            SaveFormat::Mp4 => "MP4 (video)",
        }
    }

    /// Two values, so forward and backward are the same flip.
    pub fn cycle(self) -> SaveFormat {
        match self {
            SaveFormat::Mp3 => SaveFormat::Mp4,
            SaveFormat::Mp4 => SaveFormat::Mp3,
        }
    }

    fn key(self) -> &'static str {
        match self {
            SaveFormat::Mp3 => "mp3",
            SaveFormat::Mp4 => "mp4",
        }
    }

    fn parse(text: &str) -> Option<SaveFormat> {
        match text {
            "mp3" => Some(SaveFormat::Mp3),
            "mp4" => Some(SaveFormat::Mp4),
            _ => None,
        }
    }
}

/// Height cap for `v`'s picture and for MP4 downloads.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VideoQuality {
    #[default]
    P480,
    P720,
    P1080,
    Best,
}

impl VideoQuality {
    const ALL: [VideoQuality; 4] = [
        VideoQuality::P480,
        VideoQuality::P720,
        VideoQuality::P1080,
        VideoQuality::Best,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VideoQuality::P480 => "480p",
            VideoQuality::P720 => "720p",
            VideoQuality::P1080 => "1080p",
            VideoQuality::Best => "Best",
        }
    }

    /// `None` means uncapped.
    pub fn height(self) -> Option<u16> {
        match self {
            VideoQuality::P480 => Some(480),
            VideoQuality::P720 => Some(720),
            VideoQuality::P1080 => Some(1080),
            VideoQuality::Best => None,
        }
    }

    pub fn cycle(self, forward: bool) -> VideoQuality {
        let len = Self::ALL.len();
        let at = Self::ALL.iter().position(|q| *q == self).unwrap_or(0);
        Self::ALL[(at + if forward { 1 } else { len - 1 }) % len]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub quality: VideoQuality,
    pub format: SaveFormat,
    /// Play a playlist's first track as soon as it alone resolves, and resolve the rest in
    /// the background. Only Spotify playlists resolve slowly enough to notice - YouTube and
    /// SoundCloud entries are already resolved lazily by mpv as they play.
    pub smart_loading: bool,
    /// Watch the system clipboard and queue any YouTube/SoundCloud/Spotify link that
    /// lands in it as the next track. Off by default: silently acting on the clipboard
    /// is the kind of magic a user must opt into.
    pub clipboard_watch: bool,
    /// Fade tracks into each other on playlist auto-advance instead of a hard cut.
    pub crossfade: bool,
    /// Whole-transition crossfade length, seconds, [`CROSSFADE_MIN`]..=[`CROSSFADE_MAX`].
    pub crossfade_secs: u16,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            quality: VideoQuality::default(),
            format: SaveFormat::default(),
            smart_loading: true,
            clipboard_watch: false,
            crossfade: false,
            crossfade_secs: 5,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Settings::from_kv(&text))
            .unwrap_or_default()
    }

    /// Best-effort: a settings menu that can't write its config still works for the session.
    ///
    /// Written to a sibling temp file and renamed over the config. A plain write
    /// truncates first, so a crash or a full disk mid-write would leave an empty file and
    /// lose every setting; `rename(2)` within the same directory is atomic, so the config
    /// is either the old one or the new one.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&temp, self.to_kv()).is_ok() && std::fs::rename(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }

    fn from_kv(text: &str) -> Settings {
        let mut settings = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key.trim(), value.trim()) {
                ("quality", v) => {
                    if let Some(q) = VideoQuality::ALL
                        .iter()
                        .find(|q| q.label().eq_ignore_ascii_case(v))
                    {
                        settings.quality = *q;
                    }
                }
                ("format", v) => {
                    if let Some(f) = SaveFormat::parse(v) {
                        settings.format = f;
                    }
                }
                ("smart_loading", v) => settings.smart_loading = v != "off",
                ("clipboard_watch", v) => settings.clipboard_watch = v == "on",
                ("crossfade", v) => settings.crossfade = v == "on",
                ("crossfade_secs", v) => {
                    if let Ok(secs) = v.parse::<u16>() {
                        settings.crossfade_secs = secs.clamp(CROSSFADE_MIN, CROSSFADE_MAX);
                    }
                }
                _ => {}
            }
        }
        settings
    }

    fn to_kv(self) -> String {
        format!(
            "quality={}\nformat={}\nsmart_loading={}\nclipboard_watch={}\ncrossfade={}\ncrossfade_secs={}\n",
            self.quality.label(),
            self.format.key(),
            if self.smart_loading { "on" } else { "off" },
            if self.clipboard_watch { "on" } else { "off" },
            if self.crossfade { "on" } else { "off" },
            self.crossfade_secs,
        )
    }

    /// Step the crossfade length one second, wrapping at the bounds.
    pub fn cycle_crossfade(&mut self, forward: bool) {
        self.crossfade_secs = match (forward, self.crossfade_secs) {
            (true, s) if s >= CROSSFADE_MAX => CROSSFADE_MIN,
            (true, s) => s + 1,
            (false, s) if s <= CROSSFADE_MIN => CROSSFADE_MAX,
            (false, s) => s - 1,
        };
    }
}

/// `$XDG_CONFIG_HOME/ytmplayer/config`, or `~/.config/ytmplayer/config`.
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("ytmplayer").join("config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_roundtrip_preserves_every_field() {
        let settings = Settings {
            quality: VideoQuality::P1080,
            format: SaveFormat::Mp4,
            smart_loading: false,
            clipboard_watch: true,
            crossfade: true,
            crossfade_secs: 12,
        };
        let back = Settings::from_kv(&settings.to_kv());
        assert_eq!(back.quality, VideoQuality::P1080);
        assert_eq!(back.format, SaveFormat::Mp4);
        assert!(!back.smart_loading);
        assert!(back.clipboard_watch);
        assert!(back.crossfade);
        assert_eq!(back.crossfade_secs, 12);
    }

    #[test]
    fn crossfade_secs_clamp_and_wrap() {
        // Out-of-range values in the file clamp to the bounds; garbage keeps the default.
        assert_eq!(Settings::from_kv("crossfade_secs=99").crossfade_secs, 15);
        assert_eq!(Settings::from_kv("crossfade_secs=1").crossfade_secs, 3);
        assert_eq!(Settings::from_kv("crossfade_secs=soon").crossfade_secs, 5);

        let mut s = Settings {
            crossfade_secs: CROSSFADE_MAX,
            ..Settings::default()
        };
        s.cycle_crossfade(true);
        assert_eq!(s.crossfade_secs, CROSSFADE_MIN);
        s.cycle_crossfade(false);
        assert_eq!(s.crossfade_secs, CROSSFADE_MAX);
        s.cycle_crossfade(false);
        assert_eq!(s.crossfade_secs, CROSSFADE_MAX - 1);
    }

    #[test]
    fn unknown_keys_and_garbage_fall_back_to_defaults() {
        let settings =
            Settings::from_kv("nonsense\nquality=9000p\nformat=flac\nsmart_loading=on\n");
        assert_eq!(settings.quality, VideoQuality::default());
        assert_eq!(settings.format, SaveFormat::default());
        assert!(settings.smart_loading);
    }

    #[test]
    fn quality_cycles_both_ways_and_wraps() {
        assert_eq!(VideoQuality::P480.cycle(true), VideoQuality::P720);
        assert_eq!(VideoQuality::P480.cycle(false), VideoQuality::Best);
        assert_eq!(VideoQuality::Best.cycle(true), VideoQuality::P480);
    }
}
