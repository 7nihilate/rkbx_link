// Mirrors the master deck into the shared status, for the GUI.

use crate::{beatkeeper::TrackInfo, config::Config, log::ScopedLogger, status::shared};

use super::{ModuleCreateOutput, OutputModule};

pub struct Status;

impl Status {
    pub fn create(_conf: Config, _logger: ScopedLogger) -> ModuleCreateOutput {
        Ok(Box::new(Status))
    }
}

impl OutputModule for Status {
    fn bpm_changed_master(&mut self, bpm: f32) {
        shared().update_status(|s| s.bpm = bpm);
    }

    fn beat_update_master(&mut self, beat: f32) {
        shared().update_status(|s| s.beat = beat);
    }

    fn track_changed_master(&mut self, track: &TrackInfo) {
        shared().update_status(|s| {
            s.title = track.title.clone();
            s.artist = track.artist.clone();
        });
    }

    fn phrase_changed_master(&mut self, phrase: &str) {
        shared().update_status(|s| s.phrase = phrase.to_string());
    }

    fn next_phrase_changed_master(&mut self, phrase: &str) {
        shared().update_status(|s| s.next_phrase = phrase.to_string());
    }

    fn next_phrase_in_master(&mut self, beats: i32) {
        shared().update_status(|s| s.countin = beats);
    }
}
