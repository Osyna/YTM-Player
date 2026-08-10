//! User-tunable settings, edited live in the `s` menu and persisted as plain `key=value`
//! lines. Persistence matters most for smart loading: it decides how the *next* launch
//! resolves a playlist, so a session-only flag would be pointless.
//!
//! ponytail: hand-rolled kv parsing; a config-format dependency only when settings outgrow
//! three keys.

use std::path::PathBuf;

/// Crossfade length bounds, seconds.
///
/// The value is the span of the whole transition, not the part of it where both tracks are
/// audible. Where the join falls inside that span is the score's business: a plain fade
/// puts it in the middle, so fifteen seconds is seven and a half either side of the end of
/// the outgoing track, and a sixteen-bar blend puts it three quarters of the way through.
///
/// Thirty at the top because a long blend is a real technique rather than an accident -
/// four bars either side of the join at 128 BPM is about fifteen seconds, and eight is
/// thirty. Past that the two tracks stop being mixed and start being played at once. The
/// floor is where a fade is still a fade and not a cut.
pub const CROSSFADE_MIN: u16 = 3;
pub const CROSSFADE_MAX: u16 = 30;

/// How playback moves through the queue when a track ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Repeat {
    /// Stop at the end of the queue.
    #[default]
    Off,
    /// Start the queue again from the top.
    All,
    /// Play the current track over and over.
    One,
}

impl Repeat {
    const ALL: [Repeat; 3] = [Repeat::Off, Repeat::All, Repeat::One];

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "Off",
            Repeat::All => "Queue",
            Repeat::One => "Track",
        }
    }

    pub fn cycle(self, forward: bool) -> Repeat {
        let len = Self::ALL.len();
        let at = Self::ALL.iter().position(|r| *r == self).unwrap_or(0);
        Self::ALL[(at + if forward { 1 } else { len - 1 }) % len]
    }

    fn key(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "queue",
            Repeat::One => "track",
        }
    }

    fn parse(text: &str) -> Option<Repeat> {
        match text {
            "off" => Some(Repeat::Off),
            "queue" | "all" => Some(Repeat::All),
            "track" | "one" => Some(Repeat::One),
            _ => None,
        }
    }
}

/// Loudness matching, straight from the tags most transcodes already carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Normalize {
    #[default]
    Off,
    /// Level every track to the same loudness.
    Track,
    /// Level per album, so an album's own dynamics between tracks survive.
    Album,
}

impl Normalize {
    const ALL: [Normalize; 3] = [Normalize::Off, Normalize::Track, Normalize::Album];

    pub fn label(self) -> &'static str {
        match self {
            Normalize::Off => "Off",
            Normalize::Track => "Per track",
            Normalize::Album => "Per album",
        }
    }

    /// The value mpv's `replaygain` property takes.
    pub fn mpv_value(self) -> &'static str {
        match self {
            Normalize::Off => "no",
            Normalize::Track => "track",
            Normalize::Album => "album",
        }
    }

    pub fn cycle(self, forward: bool) -> Normalize {
        let len = Self::ALL.len();
        let at = Self::ALL.iter().position(|n| *n == self).unwrap_or(0);
        Self::ALL[(at + if forward { 1 } else { len - 1 }) % len]
    }

    fn parse(text: &str) -> Option<Normalize> {
        match text {
            "no" | "off" => Some(Normalize::Off),
            "track" => Some(Normalize::Track),
            "album" => Some(Normalize::Album),
            _ => None,
        }
    }
}

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
    /// Overlap tracks on playlist auto-advance instead of cutting between them.
    pub crossfade: bool,
    /// Run the scored, beat-aware transitions rather than a plain fade.
    ///
    /// Off by default, and off is not a lesser setting - it is a clean equal-power fade
    /// between two tracks, which is what most listening wants and which costs nothing:
    /// no analysis, no decoding ahead, and video still works.
    ///
    /// On, the player measures both tracks, matches their tempo, puts them in time with
    /// each other and runs whichever score is chosen. That means deciding several seconds
    /// in advance what the next track is and reading it while this one plays, which is a
    /// different job from showing a picture - so turning this on turns video off, and the
    /// two are exclusive by design rather than by accident.
    pub beat_mixing: bool,
    /// Overlap length, seconds, [`CROSSFADE_MIN`]..=[`CROSSFADE_MAX`].
    pub crossfade_secs: u16,
    /// How one track is handed to the next. The automix applies it by itself; there is
    /// no fader to hold, which is the point - the parameters live here so the mix can
    /// run unattended.
    pub transition: crate::transitions::Style,
    /// The shape of every transition's clock, the plain fade included.
    ///
    /// One axis across the whole automix rather than a property of any one score: a
    /// score says what happens and in what order, this says when along the way. It
    /// applies with beat mixing off too - the plain equal-power fade is a score like any
    /// other, and shaping it is the one bit of transition character available to
    /// somebody who never turns the scored machinery on at all.
    pub fade_curve: crate::transitions::Curve,
    /// Play the queue in a random order.
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Level tracks against each other from their ReplayGain tags.
    ///
    /// Worth more than it looks with crossfade on: an overlap between a quiet master and
    /// a loud one is a volume jump in the middle of the transition however good the
    /// curve is.
    pub normalize: Normalize,
    /// Ask yt-dlp to tag and embed cover art in what it saves.
    pub tag_downloads: bool,
    /// Which [`crate::equalizer`] preset is on, as an index into its list.
    ///
    /// An index rather than the preset itself because the list is `const` data with no
    /// identity of its own, and because the view's cursor is an index too - one number,
    /// held once. `0` is `Flat`, which emits no filter at all.
    pub equalizer: usize,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            quality: VideoQuality::default(),
            format: SaveFormat::default(),
            smart_loading: true,
            clipboard_watch: false,
            crossfade: false,
            beat_mixing: false,
            crossfade_secs: 5,
            transition: crate::transitions::Style::default(),
            fade_curve: crate::transitions::Curve::default(),
            shuffle: false,
            repeat: Repeat::default(),
            normalize: Normalize::default(),
            tag_downloads: true,
            equalizer: 0,
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
                ("beat_mixing", v) => settings.beat_mixing = v == "on",
                ("crossfade_secs", v) => {
                    if let Ok(secs) = v.parse::<u16>() {
                        settings.crossfade_secs = secs.clamp(CROSSFADE_MIN, CROSSFADE_MAX);
                    }
                }
                ("transition", v) => {
                    if let Some(style) = crate::transitions::Style::parse(v) {
                        settings.transition = style;
                    }
                }
                ("fade_curve", v) => {
                    if let Some(curve) = crate::transitions::Curve::parse(v) {
                        settings.fade_curve = curve;
                    }
                }
                ("shuffle", v) => settings.shuffle = v == "on",
                ("repeat", v) => {
                    if let Some(r) = Repeat::parse(v) {
                        settings.repeat = r;
                    }
                }
                ("normalize", v) => {
                    if let Some(n) = Normalize::parse(v) {
                        settings.normalize = n;
                    }
                }
                ("tag_downloads", v) => settings.tag_downloads = v != "off",
                ("equalizer", v) => {
                    if let Some(at) = crate::equalizer::parse(v) {
                        settings.equalizer = at;
                    }
                }
                _ => {}
            }
        }
        settings
    }

    fn to_kv(self) -> String {
        let onoff = |flag: bool| if flag { "on" } else { "off" };
        format!(
            "quality={}\nformat={}\nsmart_loading={}\nclipboard_watch={}\ncrossfade={}\n\
             crossfade_secs={}\ntransition={}\nfade_curve={}\nbeat_mixing={}\nshuffle={}\n\
             repeat={}\nnormalize={}\ntag_downloads={}\nequalizer={}\n",
            self.quality.label(),
            self.format.key(),
            onoff(self.smart_loading),
            onoff(self.clipboard_watch),
            onoff(self.crossfade),
            self.crossfade_secs,
            self.transition.key(),
            self.fade_curve.key(),
            onoff(self.beat_mixing),
            onoff(self.shuffle),
            self.repeat.key(),
            self.normalize.mpv_value(),
            onoff(self.tag_downloads),
            crate::equalizer::at(self.equalizer).key(),
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
/// Where a user's own transitions live: `~/.config/ytmplayer/transitions`.
///
/// Beside the config rather than in the state directory because these are things a person
/// writes and would keep in a dotfile repository, which is exactly the line between the
/// two places.
pub fn transitions_dir() -> Option<PathBuf> {
    config_path().and_then(|path| path.parent().map(|dir| dir.join("transitions")))
}

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
            beat_mixing: true,
            crossfade_secs: 12,
            transition: crate::transitions::Style::parse("bass_swap").expect("a real style"),
            fade_curve: crate::transitions::Curve::Bezier,
            equalizer: crate::equalizer::parse("club").expect("a real preset"),
            shuffle: true,
            repeat: Repeat::One,
            normalize: Normalize::Album,
            tag_downloads: false,
        };
        let back = Settings::from_kv(&settings.to_kv());
        assert_eq!(back.quality, VideoQuality::P1080);
        assert_eq!(back.format, SaveFormat::Mp4);
        assert!(!back.smart_loading);
        assert!(back.clipboard_watch);
        assert!(back.crossfade);
        assert_eq!(back.crossfade_secs, 12);
        assert_eq!(back.transition.key(), "bass_swap");
        assert!(back.shuffle);
        assert_eq!(back.repeat, Repeat::One);
        assert_eq!(back.normalize, Normalize::Album);
        assert!(
            !back.tag_downloads,
            "tag_downloads defaults on, so off must survive"
        );
    }

    #[test]
    fn crossfade_secs_clamp_and_wrap() {
        // Out-of-range values in the file clamp to the bounds; garbage keeps the default.
        // Against the constants, not against a copy of them: this test has been the thing
        // that had to be edited every time the range moved, which is a test measuring the
        // number it was told rather than the behaviour it is there for.
        assert_eq!(
            Settings::from_kv("crossfade_secs=99").crossfade_secs,
            CROSSFADE_MAX
        );
        assert_eq!(
            Settings::from_kv("crossfade_secs=1").crossfade_secs,
            CROSSFADE_MIN
        );
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
