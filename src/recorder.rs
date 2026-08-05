//! The copy of the playing track that mpv writes to disk as it streams, and the policy for
//! deciding whether that copy is trustworthy enough for `d` to save without re-downloading.

use crate::mpv::Mpv;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

/// A recording is only trusted as a complete, instantly-savable copy if it started within this
/// many seconds of track start. Later attachment - after a seek, or on a playlist entry we
/// joined late - means whatever played before that point was never fed to the recorder.
const START_GRACE: f64 = 1.5;
/// mpv buffers a little of the recording internally; give it a beat to land on disk.
const FLUSH_DELAY: Duration = Duration::from_millis(600);

/// How much of the played stream has been captured, which decides whether `d` is instant.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum CacheState {
    /// Not recording: nothing has been captured, or the capture was taken or thrown away.
    #[default]
    Off,
    /// Recording, but the demuxer hasn't reached EOF yet.
    Buffering,
    /// Whole stream captured from the start; `d` can save without touching the network.
    Ready,
    /// Recording finished buffering but was attached after a seek, so it's missing whatever
    /// played before that - `d` falls back to a normal download rather than serving less than
    /// the full video.
    Partial,
}

/// Owns the in-flight recording and everything needed to judge it.
#[derive(Default)]
pub struct Recorder {
    temp: Option<PathBuf>,
    /// Distinguishes successive temp files within one session.
    counter: u32,
    /// Playlist index the current recording belongs to.
    track: i64,
    /// Playback position it was attached at.
    start_pos: f64,
    cache: CacheState,
}

impl Recorder {
    pub fn cache(&self) -> CacheState {
        self.cache
    }

    pub fn is_recording(&self) -> bool {
        self.temp.is_some()
    }

    /// The recording covers the whole track and can be saved as-is.
    pub fn is_complete(&self) -> bool {
        self.is_recording() && self.cache == CacheState::Ready
    }

    /// True once playback has moved to a different playlist entry than the one recorded.
    pub fn belongs_to_other_track(&self, playlist_pos: i64) -> bool {
        self.is_recording() && self.track != playlist_pos
    }

    /// Point mpv at a fresh temp file. Does nothing if a recording is already in flight.
    pub fn attach(&mut self, mpv: &mut Mpv, track: i64, position: f64) {
        if self.is_recording() {
            return;
        }
        self.counter += 1;
        let path = std::env::temp_dir().join(format!(
            "ytmplayer_rec_{}_{}.mkv",
            process::id(),
            self.counter
        ));
        let _ = std::fs::remove_file(&path);
        if mpv.set_stream_record(Some(&path)).is_ok() {
            self.temp = Some(path);
            self.track = track;
            self.start_pos = position;
            self.cache = CacheState::Buffering;
        }
    }

    /// Stop recording and throw the partial file away.
    pub fn discard(&mut self, mpv: &mut Mpv) {
        let _ = mpv.set_stream_record(None);
        if let Some(stale) = self.temp.take() {
            let _ = std::fs::remove_file(stale);
        }
        self.cache = CacheState::Off;
    }

    /// Re-evaluate how much of the track is on disk. Once the demuxer reaches EOF the recorder
    /// has everything it is ever going to get; whether that is the *whole* track depends on
    /// where it attached.
    pub fn refresh(&mut self, mpv: &mut Mpv) {
        if !self.is_recording() || matches!(self.cache, CacheState::Ready | CacheState::Partial) {
            return;
        }
        self.cache = if !mpv.demuxer_at_eof() {
            CacheState::Buffering
        } else if self.start_pos <= START_GRACE {
            CacheState::Ready
        } else {
            CacheState::Partial
        };
    }

    /// Release the finished recording to be saved, stopping mpv's recorder first.
    pub fn take(&mut self, mpv: &mut Mpv) -> Option<PathBuf> {
        let temp = self.temp.take()?;
        let _ = mpv.set_stream_record(None);
        std::thread::sleep(FLUSH_DELAY);
        self.cache = CacheState::Off;
        Some(temp)
    }
}
