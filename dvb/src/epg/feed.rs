// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reads the programme guide from the stream that is playing.
//!
//! The FRITZ!Box adds the service information to every channel, so while a
//! channel plays (and the sweep pauses), its multiplex comes for free: a
//! [`LiveFeed`] is one more consumer of the [`Relay`](crate::Relay) that
//! collects the PAT, SDT and EIT like a scan does, and adds them to the
//! guide every few seconds. As this also records that the multiplex was
//! scanned, the sweep doesn't visit it right after playback.
//!
//! The section parsing happens in the task of the feed, not in the receive
//! loop of the relay. If the feed falls behind, it loses chunks like any
//! consumer, and with them at most a few sections, which are repeated.

use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{broadcast, broadcast::error::RecvError, watch},
    task::JoinHandle,
    time::{interval_at, Instant, MissedTickBehavior},
};

use super::{identity, mux::Mux, scan::Collector, sweep::unix_now, Inner};
use crate::{
    si::{PID_EIT, PID_PAT, PID_SDT},
    ts::SectionAssembler,
    Chunk,
};

/// Adds the programme guide of the stream that is playing to the guide.
/// Created by [`super::Epg::feed`].
///
/// It ends when the relay ends (the channel is closed), on
/// [`LiveFeed::stop`] or when it is dropped, and adds what it collected
/// before.
#[derive(Debug)]
pub struct LiveFeed {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl LiveFeed {
    pub(super) fn start(
        inner: Arc<Inner>,
        url: &str,
        receiver: broadcast::Receiver<Chunk>,
    ) -> Self {
        let mux = inner
            .muxes
            .lock()
            .unwrap()
            .iter()
            .find(|mux| {
                mux.channels
                    .iter()
                    .any(|channel| channel.identity == identity(url))
            })
            .cloned();
        if mux.is_none() {
            tracing::debug!("not reading the programme guide of the unknown channel {url}");
        }

        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(inner, mux, receiver, stopped));
        Self { stop, task }
    }

    /// Stops reading the stream, after adding what was collected so far.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Returns `true` once the feed has ended, e.g. with the relay.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Waits until the feed has ended, e.g. after [`LiveFeed::stop`].
    pub async fn join(mut self) {
        let _ = (&mut self.task).await;
    }
}

/// Resolves when `stop` is set or the feed was dropped.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

async fn run(
    inner: Arc<Inner>,
    mux: Option<Mux>,
    mut receiver: broadcast::Receiver<Chunk>,
    mut stop: watch::Receiver<bool>,
) {
    let Some(mux) = mux else {
        // keep the channel drained until the end
        loop {
            tokio::select! {
                () = stopped(&mut stop) => return,
                received = receiver.recv() => if let Err(RecvError::Closed) = received {
                    return;
                },
            }
        }
    };

    // the cache must not replace what the feed adds
    inner.load_cache().await;

    let timing = inner.timing;
    let mut assembler = SectionAssembler::new(&[PID_PAT, PID_SDT, PID_EIT]);
    let mut collector = Collector::new(mux.channel_pids().collect());
    let mut feed = Feed {
        inner: &inner,
        mux: &mux,
        changed: false,
        unsaved: false,
        last_save: None,
        save_interval: timing.feed_save,
    };
    let mut update = interval_at(Instant::now() + timing.feed_update, timing.feed_update);
    update.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            () = stopped(&mut stop) => break,
            received = receiver.recv() => match received {
                Ok(chunk) => {
                    feed.changed |= collector.push(&chunk, &mut assembler, Instant::now());
                }
                // the tables are repeated
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            _ = update.tick() => feed.update(&collector).await,
        }
    }

    feed.update(&collector).await;
    if feed.unsaved {
        feed.inner.save_cache().await;
    }
}

/// Adds the collected tables to the guide.
struct Feed<'a> {
    inner: &'a Inner,
    mux: &'a Mux,
    /// New tables were collected since the last update.
    changed: bool,
    /// The guide changed since the cache was saved.
    unsaved: bool,
    last_save: Option<Instant>,
    save_interval: Duration,
}

impl Feed<'_> {
    async fn update(&mut self, collector: &Collector) {
        if !self.changed {
            return;
        }
        self.changed = false;

        // the guide is off
        let Some(notify) = self.inner.notify.lock().unwrap().clone() else {
            return;
        };

        let scan = collector.snapshot();
        let now = unix_now();
        {
            let mut store = self.inner.store();
            store.add_scan(self.mux, &scan, now);
            store.prune(now);
        }
        tracing::debug!(
            "{}: {} EIT sections from the stream",
            self.mux.key,
            scan.eit.len()
        );
        let _ = notify.send(());
        self.unsaved = true;

        if self
            .last_save
            .is_none_or(|last| last.elapsed() >= self.save_interval)
        {
            self.inner.save_cache().await;
            self.last_save = Some(Instant::now());
            self.unsaved = false;
        }
    }
}
