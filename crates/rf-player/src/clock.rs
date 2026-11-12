//! Oynatma saati: ses varsa hoparlörden çıkan örnek sayısı, yoksa duvar saati.

use std::time::Instant;

use crate::audio_out::AudioSink;

/// Bir andaki saat değeri (saniye).
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    secs: f64,
}

impl Clock {
    /// Ses çıkışının o ana kadar çaldığı süre.
    pub fn audio<S: AudioSink + ?Sized>(sink: &S) -> Clock {
        Clock { secs: sink.played_frames() as f64 / sink.sample_rate().max(1) as f64 }
    }

    /// `since` anından bu yana geçen duvar saati süresi.
    pub fn wall(since: Instant) -> Clock {
        Clock { secs: since.elapsed().as_secs_f64() }
    }

    pub fn now(&self) -> f64 {
        self.secs
    }
}
