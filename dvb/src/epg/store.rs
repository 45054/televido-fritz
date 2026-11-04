// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! The programme data collected from the multiplexes.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    mux::{match_services, Mux},
    scan::Scan,
};
use crate::si::{Eit, ServiceKey};

/// Events starting later than this are not kept.
const HORIZON: i64 = 12 * 3600;
/// Number of upcoming events kept per service.
const MAX_EVENTS: usize = 8;
/// A multiplex is not scanned again within this time.
const MIN_RESCAN: i64 = 5 * 60;
/// Services without any events are checked again after this time.
const NO_DATA_RETRY: i64 = 30 * 60;
/// Data older than this is refreshed.
const MAX_AGE: i64 = 45 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: u16,
    /// Unix timestamp.
    pub start: i64,
    /// Unix timestamp.
    pub end: i64,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
}

/// The running and the following event of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowNext {
    pub current: Option<Event>,
    pub next: Option<Event>,
}

/// Why a multiplex should be scanned, from the least to the most urgent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// The data is old, or a channel without data is checked again.
    Refresh,
    /// A channel shows no programme: the multiplex was never scanned, or the
    /// events of a channel ran out.
    Missing,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ServiceEvents {
    /// Sorted by start time, without overlaps.
    events: Vec<Event>,
    /// When events were last received.
    updated: i64,
}

impl ServiceEvents {
    fn current(&self, now: i64) -> Option<&Event> {
        self.events
            .iter()
            .find(|event| event.start <= now && now < event.end)
    }
}

#[derive(Clone, Debug, Default)]
pub struct EpgStore {
    services: HashMap<ServiceKey, ServiceEvents>,
    /// The service of every channel, by [`super::mux::identity`].
    channels: HashMap<String, ServiceKey>,
    /// When every multiplex was scanned the last time, by [`Mux::key`].
    scans: HashMap<String, i64>,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    services: Vec<(ServiceKey, ServiceEvents)>,
    channels: Vec<(String, ServiceKey)>,
    scans: Vec<(String, i64)>,
}

impl EpgStore {
    pub fn from_json(json: &[u8], now: i64) -> Self {
        let Ok(snapshot) = serde_json::from_slice::<Snapshot>(json) else {
            return Self::default();
        };

        let mut store = Self {
            services: snapshot.services.into_iter().collect(),
            channels: snapshot.channels.into_iter().collect(),
            scans: snapshot.scans.into_iter().collect(),
        };
        store.prune(now);
        store
    }

    /// Serializes the data of the known channels. Events of services that
    /// were only announced by other multiplexes are dropped.
    pub fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        let known: HashSet<_> = self.channels.values().collect();

        serde_json::to_vec(&Snapshot {
            services: self
                .services
                .iter()
                .filter(|(key, _)| known.contains(key))
                .map(|(key, events)| (*key, events.clone()))
                .collect(),
            channels: self
                .channels
                .iter()
                .map(|(identity, key)| (identity.clone(), *key))
                .collect(),
            scans: self
                .scans
                .iter()
                .map(|(key, time)| (key.clone(), *time))
                .collect(),
        })
    }

    /// Adds the result of a scan of `mux`.
    pub fn add_scan(&mut self, mux: &Mux, scan: &Scan, now: i64) {
        if let (Some(pat), Some(original_network_id)) = (&scan.pat, scan.original_network_id()) {
            for (identity, service) in
                match_services(mux, original_network_id, pat, scan.sdt.as_ref())
            {
                self.channels.insert(identity, service);
            }
            // without the tables nothing was received from the multiplex
            self.scans.insert(mux.key.clone(), now);
        }
        for eit in &scan.eit {
            self.add_eit(eit, now);
        }
    }

    /// Adds the events of an EIT section. Events that overlap with new ones
    /// are replaced.
    pub fn add_eit(&mut self, eit: &Eit, now: i64) {
        let events: Vec<_> = eit
            .events
            .iter()
            .filter(|event| event.end > now && event.start < now + HORIZON)
            .collect();
        if events.is_empty() {
            return;
        }

        let service = self.services.entry(eit.service).or_default();
        for new in events {
            service.events.retain(|known| {
                known.id != new.event_id && (known.end <= new.start || new.end <= known.start)
            });
            service.events.push(Event {
                id: new.event_id,
                start: new.start,
                end: new.end,
                title: new.title.clone(),
                subtitle: new.subtitle.clone(),
                description: new.description.clone(),
            });
        }
        service.events.sort_by_key(|event| event.start);
        service.events.truncate(MAX_EVENTS);
        service.updated = now;
    }

    /// Removes events that have ended.
    ///
    /// The services of known channels are kept without events, which tells
    /// channels whose events ran out from channels without any.
    pub fn prune(&mut self, now: i64) {
        let known: HashSet<_> = self.channels.values().copied().collect();
        for service in self.services.values_mut() {
            service.events.retain(|event| event.end > now);
        }
        self.services
            .retain(|key, service| !service.events.is_empty() || known.contains(key));
    }

    /// Returns the running and the following event of a channel.
    pub fn now_next(&self, identity: &str, now: i64) -> Option<NowNext> {
        let service = self.services.get(self.channels.get(identity)?)?;
        let current = service.current(now).cloned();
        let following_start = current.as_ref().map_or(now, |current| current.end);
        let next = service
            .events
            .iter()
            .find(|event| event.start >= following_start && event.start > now)
            .cloned();

        (current.is_some() || next.is_some()).then_some(NowNext { current, next })
    }

    /// Returns whether scanning `mux` is useful, and how urgent it is: it was
    /// never scanned, or the events of its channels ran out or are old.
    ///
    /// Multiplexes that announce the events of other multiplexes ("EIT
    /// other") keep the data of all channels up to date, so that they don't
    /// need to be scanned themselves.
    pub fn scan_urgency(&self, mux: &Mux, now: i64) -> Option<Urgency> {
        let Some(&last_scan) = self.scans.get(&mux.key) else {
            return Some(Urgency::Missing);
        };
        let since_scan = now - last_scan;
        if since_scan < MIN_RESCAN {
            return None;
        }

        mux.channels
            .iter()
            .filter_map(|channel| {
                let events = self
                    .channels
                    .get(&channel.identity)
                    .and_then(|service| self.services.get(service));
                match events {
                    // the service doesn't provide events, or isn't known
                    None => (since_scan >= NO_DATA_RETRY).then_some(Urgency::Refresh),
                    Some(events) if events.events.iter().all(|event| event.end <= now) => {
                        if events.updated >= last_scan {
                            // the events ran out, although the last scan
                            // still provided some
                            Some(Urgency::Missing)
                        } else {
                            // the last scan provided no events anymore
                            (since_scan >= NO_DATA_RETRY).then_some(Urgency::Refresh)
                        }
                    }
                    Some(events) => (events.current(now).is_none()
                        || now - events.updated >= MAX_AGE)
                        .then_some(Urgency::Refresh),
                }
            })
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{mux::group, scan::Scan},
        *,
    };
    use crate::{
        si::{EitEvent, Pat},
        FritzChannel,
    };

    const NOW: i64 = 1_700_000_000;
    const URL: &str = "rtsp://192.168.178.1:554/?avm=1&freq=450&msys=dvbc&mtype=256qam&sr=6900&pids=0,16,17,18,20,100,110";

    fn mux() -> Mux {
        group(&[FritzChannel {
            name: "Channel".to_owned(),
            url: URL.to_owned(),
        }])
        .remove(0)
    }

    fn service() -> ServiceKey {
        ServiceKey {
            original_network_id: 1,
            transport_stream_id: 2,
            service_id: 3,
        }
    }

    fn event(id: u16, start: i64, end: i64, title: &str) -> EitEvent {
        EitEvent {
            event_id: id,
            start,
            end,
            title: title.to_owned(),
            subtitle: None,
            description: Some(format!("About {title}")),
        }
    }

    fn eit(section_number: u8, events: Vec<EitEvent>) -> Eit {
        Eit {
            table_id: 0x4E,
            service: service(),
            version: 0,
            section_number,
            events,
        }
    }

    fn scan(events: Vec<EitEvent>) -> Scan {
        Scan {
            pat: Some(Pat {
                transport_stream_id: 2,
                programs: vec![(3, 100)],
            }),
            sdt: None,
            eit: vec![eit(0, events)],
        }
    }

    fn identity() -> String {
        mux().channels[0].identity.clone()
    }

    #[test]
    fn returns_current_and_next_event() {
        let mut store = EpgStore::default();
        // the service of the channel is unknown before the first scan
        store.add_eit(&eit(0, vec![event(1, NOW - 60, NOW + 60, "A")]), NOW);
        assert_eq!(store.now_next(&identity(), NOW), None);

        store.add_scan(
            &mux(),
            &scan(vec![
                event(1, NOW - 60, NOW + 60, "A"),
                event(2, NOW + 60, NOW + 600, "B"),
            ]),
            NOW,
        );

        let now_next = store.now_next(&identity(), NOW).unwrap();
        assert_eq!(now_next.current.unwrap().title, "A");
        assert_eq!(now_next.next.unwrap().title, "B");

        // the next event becomes current without new data
        let now_next = store.now_next(&identity(), NOW + 120).unwrap();
        assert_eq!(now_next.current.unwrap().title, "B");
        assert_eq!(now_next.next, None);

        // everything has ended
        assert_eq!(store.now_next(&identity(), NOW + 700), None);
        assert_eq!(store.now_next("unknown", NOW), None);
    }

    #[test]
    fn finds_next_event_without_current_one() {
        let mut store = EpgStore::default();
        store.add_scan(
            &mux(),
            &scan(vec![event(2, NOW + 600, NOW + 900, "B")]),
            NOW,
        );

        let now_next = store.now_next(&identity(), NOW).unwrap();
        assert_eq!(now_next.current, None);
        assert_eq!(now_next.next.unwrap().title, "B");
    }

    #[test]
    fn replaces_overlapping_events() {
        let mut store = EpgStore::default();
        store.add_scan(
            &mux(),
            &scan(vec![
                event(1, NOW - 60, NOW + 60, "Old"),
                event(2, NOW + 60, NOW + 600, "Later"),
            ]),
            NOW,
        );
        // the broadcaster changed the schedule: a longer event with a new ID
        store.add_eit(&eit(0, vec![event(5, NOW - 60, NOW + 300, "New")]), NOW + 1);
        // and updated an event with the same ID
        store.add_eit(
            &eit(1, vec![event(2, NOW + 300, NOW + 900, "Later!")]),
            NOW + 1,
        );

        let now_next = store.now_next(&identity(), NOW).unwrap();
        assert_eq!(now_next.current.unwrap().title, "New");
        let next = now_next.next.unwrap();
        assert_eq!((next.title.as_str(), next.end), ("Later!", NOW + 900));
    }

    #[test]
    fn ignores_ended_and_distant_events() {
        let mut store = EpgStore::default();
        store.add_scan(
            &mux(),
            &scan(vec![
                event(1, NOW - 600, NOW - 1, "Ended"),
                event(2, NOW + HORIZON + 1, NOW + HORIZON + 100, "Far away"),
            ]),
            NOW,
        );
        assert_eq!(store.now_next(&identity(), NOW), None);
    }

    #[test]
    fn decides_which_multiplexes_to_scan() {
        let mux = mux();
        let mut store = EpgStore::default();
        assert_eq!(
            store.scan_urgency(&mux, NOW),
            Some(Urgency::Missing),
            "never scanned"
        );

        store.add_scan(&mux, &scan(vec![event(1, NOW - 60, NOW + 600, "A")]), NOW);
        assert_eq!(store.scan_urgency(&mux, NOW + 10), None, "just scanned");
        assert_eq!(store.scan_urgency(&mux, NOW + 400), None, "data is current");

        // the events ran out
        assert_eq!(
            store.scan_urgency(&mux, NOW + 601 + MIN_RESCAN),
            Some(Urgency::Missing),
            "data ran out"
        );

        // data that EIT other of another multiplex provides keeps it fresh
        store.add_eit(
            &eit(0, vec![event(2, NOW + 600, NOW + 5000, "B")]),
            NOW + 700,
        );
        assert_eq!(store.scan_urgency(&mux, NOW + 1000), None);
        // but not forever
        assert_eq!(
            store.scan_urgency(&mux, NOW + 700 + MAX_AGE),
            Some(Urgency::Refresh)
        );
    }

    #[test]
    fn refreshes_channels_between_events() {
        let mux = mux();
        let mut store = EpgStore::default();
        store.add_scan(
            &mux,
            &scan(vec![event(1, NOW + 600, NOW + 900, "Later")]),
            NOW,
        );
        assert_eq!(
            store.scan_urgency(&mux, NOW + MIN_RESCAN),
            Some(Urgency::Refresh)
        );
    }

    #[test]
    fn prioritizes_channels_whose_events_ran_out() {
        let mux = mux();
        let mut store = EpgStore::default();
        store.add_scan(&mux, &scan(vec![event(1, NOW - 60, NOW + 600, "A")]), NOW);

        // the scan of another multiplex removes the ended events
        store.prune(NOW + 700);
        assert_eq!(store.now_next(&identity(), NOW + 700), None);
        assert_eq!(store.scan_urgency(&mux, NOW + 700), Some(Urgency::Missing));

        // the same after a restart
        let loaded = EpgStore::from_json(&store.to_json().unwrap(), NOW + 700);
        assert_eq!(loaded.scan_urgency(&mux, NOW + 700), Some(Urgency::Missing));

        // if the next scan provides no events, the service is checked rarely
        store.add_scan(&mux, &scan(vec![]), NOW + 700);
        assert_eq!(store.scan_urgency(&mux, NOW + 700 + MIN_RESCAN), None);
        assert_eq!(
            store.scan_urgency(&mux, NOW + 700 + NO_DATA_RETRY),
            Some(Urgency::Refresh)
        );
    }

    #[test]
    fn rescans_services_without_events_rarely() {
        let mux = mux();
        let mut store = EpgStore::default();
        store.add_scan(&mux, &scan(vec![]), NOW);

        assert_eq!(store.scan_urgency(&mux, NOW + MIN_RESCAN + 1), None);
        assert_eq!(
            store.scan_urgency(&mux, NOW + NO_DATA_RETRY),
            Some(Urgency::Refresh)
        );
    }

    #[test]
    fn does_not_remember_scans_without_data() {
        let mux = mux();
        let mut store = EpgStore::default();
        store.add_scan(&mux, &Scan::default(), NOW);
        assert_eq!(store.scan_urgency(&mux, NOW + 1), Some(Urgency::Missing));
    }

    #[test]
    fn keeps_a_limited_number_of_events() {
        let mut store = EpgStore::default();
        let events = (0..20)
            .map(|i| event(i, NOW + i as i64 * 100, NOW + (i as i64 + 1) * 100, "Show"))
            .collect();
        store.add_scan(&mux(), &scan(events), NOW);

        assert_eq!(store.services[&service()].events.len(), MAX_EVENTS);
        // the earliest ones are kept
        assert_eq!(store.services[&service()].events[0].id, 0);
    }

    #[test]
    fn only_caches_services_of_known_channels() {
        let mut store = EpgStore::default();
        store.add_scan(&mux(), &scan(vec![event(1, NOW - 60, NOW + 60, "A")]), NOW);

        let other = Eit {
            service: ServiceKey {
                original_network_id: 9,
                transport_stream_id: 9,
                service_id: 9,
            },
            ..eit(0, vec![event(2, NOW - 60, NOW + 60, "Other")])
        };
        store.add_eit(&other, NOW);
        assert_eq!(store.services.len(), 2);

        let loaded = EpgStore::from_json(&store.to_json().unwrap(), NOW);
        assert_eq!(loaded.services.len(), 1);
        assert!(loaded.services.contains_key(&service()));

        // after the events ended, only the service of the channel is kept
        store.prune(NOW + 100);
        assert_eq!(store.services.len(), 1);
        assert!(store.services[&service()].events.is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let mut store = EpgStore::default();
        store.add_scan(
            &mux(),
            &scan(vec![
                event(1, NOW - 60, NOW + 60, "A"),
                event(2, NOW + 60, NOW + 600, "B"),
            ]),
            NOW,
        );

        let json = store.to_json().unwrap();
        // events that ended in the meantime are dropped
        let loaded = EpgStore::from_json(&json, NOW + 100);
        let now_next = loaded.now_next(&identity(), NOW + 100).unwrap();
        assert_eq!(now_next.current.unwrap().title, "B");
        assert_eq!(loaded.scan_urgency(&mux(), NOW + 100), None);

        assert!(EpgStore::from_json(b"{ not json", NOW).services.is_empty());
        assert!(EpgStore::from_json(b"", NOW).channels.is_empty());
    }
}
