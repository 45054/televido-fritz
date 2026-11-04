// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shares the tuner of the FRITZ!Box between playback and reading the
//! programme guide.
//!
//! The FRITZ!Box delivers only one stream per device, so the player of the
//! application has to announce playback (see [`Tuner::begin_playback`]).
//! Playback always has priority: scanning stops as soon as playback starts
//! and resumes after it ended.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::{
    sync::{watch, Mutex, MutexGuard},
    time::timeout,
};

/// Time that playback waits for a running scan to end its session.
const SCAN_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Coordinates the users of the tuner of the FRITZ!Box.
#[derive(Debug)]
pub struct Tuner {
    playing: watch::Sender<bool>,
    /// Counts the calls of `begin_playback`, so that the end of a playback
    /// that was already replaced by another one doesn't end the new one.
    /// Only changed while the lock of `playing` is held.
    generation: AtomicU64,
    /// Held while a scan has a session with the FRITZ!Box.
    session: Mutex<()>,
}

impl Default for Tuner {
    fn default() -> Self {
        Self {
            playing: watch::Sender::new(false),
            generation: AtomicU64::new(0),
            session: Mutex::new(()),
        }
    }
}

impl Tuner {
    /// Stops scanning and waits until the FRITZ!Box has released the tuner.
    ///
    /// Call this before starting playback, and [`Tuner::end_playback`] when
    /// the playback has ended. Starting another channel while a channel is
    /// playing doesn't need an `end_playback` in between.
    pub async fn begin_playback(&self) {
        self.begin_playback_generation().await;
    }

    /// Like [`Tuner::begin_playback`], and returns the generation for
    /// [`Tuner::end_playback_of`].
    pub(crate) async fn begin_playback_generation(&self) -> u64 {
        let mut generation = 0;
        self.playing.send_modify(|playing| {
            generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            *playing = true;
        });
        if timeout(SCAN_STOP_TIMEOUT, self.session.lock())
            .await
            .is_err()
        {
            tracing::warn!("the programme guide scan didn't stop in time");
        }
        generation
    }

    /// Announces that playback has ended, which lets the programme guide
    /// use the tuner again.
    pub fn end_playback(&self) {
        self.playing.send_replace(false);
    }

    /// Ends the playback started by the [`Tuner::begin_playback_generation`]
    /// that returned `generation`, unless another playback started since.
    pub(crate) fn end_playback_of(&self, generation: u64) {
        self.playing.send_if_modified(|playing| {
            let current = self.generation.load(Ordering::SeqCst) == generation;
            let changed = current && *playing;
            if current {
                *playing = false;
            }
            changed
        });
    }

    /// Returns whether a channel is playing (or recording), which keeps
    /// the programme guide from using the tuner.
    pub fn is_playing(&self) -> bool {
        *self.playing.borrow()
    }

    /// Resolves as soon as playback starts.
    pub(super) async fn playback_started(&self) {
        let _ = self.playing.subscribe().wait_for(|playing| *playing).await;
    }

    /// Resolves as soon as no channel is playing.
    pub(super) async fn playback_ended(&self) {
        let _ = self.playing.subscribe().wait_for(|playing| !*playing).await;
    }

    /// Blocks until no scan has a session with the FRITZ!Box, at most for
    /// `timeout`.
    pub(super) fn wait_until_released(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.session.try_lock().is_err() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Takes the right to use the tuner for a scan.
    pub(super) async fn lock_session(&self) -> MutexGuard<'_, ()> {
        self.session.lock().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn playback_waits_for_the_session() {
        let tuner = Arc::new(Tuner::default());
        let session = tuner.lock_session().await;

        let task = tokio::spawn({
            let tuner = tuner.clone();
            async move { tuner.begin_playback().await }
        });
        // the scan is told to stop
        tokio::time::timeout(Duration::from_secs(1), tuner.playback_started())
            .await
            .unwrap();
        assert!(tuner.is_playing());
        assert!(!task.is_finished());

        drop(session);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        tuner.end_playback();
        tokio::time::timeout(Duration::from_secs(1), tuner.playback_ended())
            .await
            .unwrap();
        assert!(!tuner.is_playing());
    }

    #[tokio::test]
    async fn the_end_of_a_replaced_playback_is_ignored() {
        let tuner = Tuner::default();
        let first = tuner.begin_playback_generation().await;
        let second = tuner.begin_playback_generation().await;

        tuner.end_playback_of(first);
        assert!(tuner.is_playing(), "the second playback goes on");
        tuner.end_playback_of(second);
        assert!(!tuner.is_playing());
    }
}
