// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Keeps the programme guide up to date by visiting the multiplexes while
//! the tuner is idle.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::{mpsc::UnboundedSender, watch},
    time::{sleep, Instant},
};

use super::{
    mux::Mux,
    scan::{self, ScanError},
    store::EpgStore,
    Inner,
};
use crate::rtsp::StatusError;

/// Timing of the sweep.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Pause after playback ended before the tuner is used again.
    pub settle: Duration,
    /// How often is checked whether a multiplex needs to be scanned.
    pub poll: Duration,
    /// Pause before a multiplex that failed is tried again.
    pub retry: Duration,
    /// How often a [`super::LiveFeed`] adds new tables to the guide.
    pub feed_update: Duration,
    /// How often a [`super::LiveFeed`] saves the cache at most.
    pub feed_save: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(3),
            poll: Duration::from_secs(60),
            retry: Duration::from_secs(5 * 60),
            feed_update: Duration::from_secs(5),
            feed_save: Duration::from_secs(60),
        }
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// Resolves when the sender of `cancel` was dropped.
async fn cancelled(cancel: &mut watch::Receiver<()>) {
    while cancel.changed().await.is_ok() {}
}

fn is_cancelled(cancel: &watch::Receiver<()>) -> bool {
    cancel.has_changed().is_err()
}

/// Returns the multiplex to scan next: the most urgent one, and of equally
/// urgent ones the first, so that the channels at the top of the list are
/// filled first. Multiplexes that failed are skipped until they are retried.
fn next_mux<'a>(
    store: &EpgStore,
    muxes: &'a [Mux],
    retry_at: &HashMap<String, Instant>,
    now: i64,
    instant: Instant,
) -> Option<&'a Mux> {
    muxes
        .iter()
        .filter(|mux| retry_at.get(&mux.key).is_none_or(|retry| *retry <= instant))
        .filter_map(|mux| Some((store.scan_urgency(mux, now)?, mux)))
        // unlike `max_by_key`, `min_by_key` returns the first of equal ones
        .min_by_key(|(urgency, _)| Reverse(*urgency))
        .map(|(_, mux)| mux)
}

/// Scans `muxes`, the channels grouped by `mux::group`, until `cancel` is
/// dropped.
///
/// After every scan, the next multiplex is chosen anew: channels without
/// programme information come first, and the events of other multiplexes
/// ("EIT other") that a scan provided are taken into account.
pub async fn run(
    inner: Arc<Inner>,
    muxes: Arc<[Mux]>,
    mut cancel: watch::Receiver<()>,
    notify: UnboundedSender<()>,
) {
    inner.load_cache().await;
    let _ = notify.send(());

    let timing = inner.timing;
    // multiplexes that failed, and when to try them again
    let mut retry_at: HashMap<String, Instant> = HashMap::new();
    // multiplexes that only work with the full list of PIDs
    let mut full_pids: HashSet<String> = HashSet::new();

    loop {
        if is_cancelled(&cancel) {
            return;
        }

        // let playback use the tuner
        while inner.tuner.is_playing() {
            tokio::select! {
                _ = cancelled(&mut cancel) => return,
                _ = inner.tuner.playback_ended() => {}
            }
            tokio::select! {
                _ = cancelled(&mut cancel) => return,
                _ = sleep(timing.settle) => {}
            }
        }

        let next = {
            let store = inner.store();
            next_mux(&store, &muxes, &retry_at, unix_now(), Instant::now())
        };
        let Some(mux) = next else {
            tokio::select! {
                _ = cancelled(&mut cancel) => return,
                _ = sleep(timing.poll) => {}
            }
            continue;
        };

        let url = if full_pids.contains(&mux.key) {
            &mux.fallback_url
        } else {
            &mux.scan_url
        };
        tracing::debug!("reading the programme guide of {}", mux.key);

        let guard = inner.tuner.lock_session().await;
        if inner.tuner.is_playing() {
            continue;
        }
        let result = {
            let mut cancel = cancel.clone();
            scan::scan(url, mux.channel_pids().collect(), async {
                tokio::select! {
                    _ = inner.tuner.playback_started() => {}
                    _ = cancelled(&mut cancel) => {}
                }
            })
            .await
        };
        drop(guard);

        match result {
            Ok(scan) => {
                let now = unix_now();
                // maybe the FRITZ!Box only starts the stream if it contains
                // audio or video
                if scan.pat.is_none()
                    && !full_pids.contains(&mux.key)
                    && mux.scan_url != mux.fallback_url
                {
                    tracing::info!(
                        "received no service information from {}, trying again with all PIDs",
                        mux.key
                    );
                    full_pids.insert(mux.key.clone());
                    continue;
                }
                // the scan isn't remembered without the tables that identify
                // the multiplex, so it would be chosen again right away
                if scan.original_network_id().is_none() {
                    tracing::warn!("received no usable service information from {}", mux.key);
                    retry_at.insert(mux.key.clone(), Instant::now() + timing.retry);
                }
                tracing::debug!(
                    "{}: {} services, {} EIT sections",
                    mux.key,
                    scan.pat.as_ref().map_or(0, |pat| pat.programs.len()),
                    scan.eit.len()
                );
                {
                    let mut store = inner.store();
                    store.add_scan(mux, &scan, now);
                    store.prune(now);
                }
                inner.save_cache().await;
                let _ = notify.send(());
            }
            // playback started or the sweep was cancelled
            Err(ScanError::Stopped) => continue,
            Err(ScanError::Failed(e)) => {
                let rejected = e.downcast_ref::<StatusError>().is_some();
                if rejected && !full_pids.contains(&mux.key) && mux.scan_url != mux.fallback_url {
                    tracing::info!(
                        "{} rejected the stream without audio and video, trying again with all PIDs: {e:#}",
                        mux.key
                    );
                    full_pids.insert(mux.key.clone());
                    continue;
                }
                tracing::warn!("failed to read the programme guide of {}: {e:#}", mux.key);
                retry_at.insert(mux.key.clone(), Instant::now() + timing.retry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{mux, scan::Scan, store::Urgency},
        *,
    };
    use crate::{
        si::{Eit, EitEvent, Pat, ServiceKey},
        FritzChannel,
    };

    const NOW: i64 = 1_700_000_000;

    /// Three multiplexes with one channel each.
    fn muxes() -> Vec<Mux> {
        mux::group(&[450, 338, 594].map(|frequency| FritzChannel {
            name: format!("Channel {frequency}"),
            url: format!(
                "rtsp://192.168.178.1:554/?avm=1&freq={frequency}&msys=dvbc&mtype=256qam&sr=6900&pids=0,16,17,18,20,100,110"
            ),
        }))
    }

    /// A scan at `time` that provides a long event of the channel.
    fn scan(transport_stream_id: u16, time: i64) -> Scan {
        Scan {
            pat: Some(Pat {
                transport_stream_id,
                programs: vec![(3, 100)],
            }),
            sdt: None,
            eit: vec![Eit {
                table_id: 0x4E,
                service: ServiceKey {
                    original_network_id: 1,
                    transport_stream_id,
                    service_id: 3,
                },
                version: 0,
                section_number: 0,
                events: vec![EitEvent {
                    event_id: 1,
                    start: time - 60,
                    end: time + 24 * 3600,
                    title: "Show".to_owned(),
                    subtitle: None,
                    description: None,
                }],
            }],
        }
    }

    #[test]
    fn scans_channels_without_programme_first() {
        let muxes = muxes();
        let instant = Instant::now();
        let mut retry_at = HashMap::new();
        let mut store = EpgStore::default();
        let next = |store: &EpgStore, retry_at: &HashMap<String, Instant>, instant| {
            next_mux(store, &muxes, retry_at, NOW, instant).map(|mux| mux.key.clone())
        };

        // the data of the first multiplex is old, the others have none
        store.add_scan(&muxes[0], &scan(1, NOW - 3600), NOW - 3600);
        assert_eq!(store.scan_urgency(&muxes[0], NOW), Some(Urgency::Refresh));
        assert_eq!(
            next(&store, &retry_at, instant),
            Some(muxes[1].key.clone()),
            "the first multiplex without data comes first"
        );

        // multiplexes that failed are skipped until they are retried
        retry_at.insert(muxes[1].key.clone(), instant + Duration::from_secs(60));
        assert_eq!(next(&store, &retry_at, instant), Some(muxes[2].key.clone()));
        assert_eq!(
            next(&store, &retry_at, instant + Duration::from_secs(60)),
            Some(muxes[1].key.clone())
        );

        // old data is refreshed afterwards
        store.add_scan(&muxes[1], &scan(2, NOW), NOW);
        store.add_scan(&muxes[2], &scan(3, NOW), NOW);
        assert_eq!(next(&store, &retry_at, instant), Some(muxes[0].key.clone()));

        store.add_scan(&muxes[0], &scan(1, NOW), NOW);
        assert_eq!(next(&store, &retry_at, instant), None);
    }
}
