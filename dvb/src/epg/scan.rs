// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reads the programme information of one multiplex.
//!
//! Tunes the multiplex with an RTSP session that only requests the PIDs of
//! the service information (PAT, SDT, EIT) and collects the tables until the
//! present/following events of the wanted services are complete.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use tokio::time::{timeout, Instant};

use crate::{
    rtsp::Stream,
    si::{Eit, Pat, Sdt, ServiceKey, PID_EIT, PID_PAT, PID_SDT},
    ts::SectionAssembler,
};

/// The scan stops after this time, whatever was received.
const MAX_DURATION: Duration = Duration::from_secs(8);
/// The scan stops if no new present/following section was received for this
/// long (the actual transport stream repeats them every 2 seconds).
const QUIET_TIME: Duration = Duration::from_millis(3500);

/// Collects the tables of a multiplex.
#[derive(Debug)]
pub struct Collector {
    /// PMT PIDs of the services the caller is interested in.
    wanted_pmt_pids: HashSet<u16>,
    pub pat: Option<Pat>,
    pub sdt: Option<Sdt>,
    /// The newest version of every section.
    sections: HashMap<(u8, ServiceKey, u8), Eit>,
    last_new_present_following: Option<Instant>,
}

impl Collector {
    pub fn new(wanted_pmt_pids: HashSet<u16>) -> Self {
        Self {
            wanted_pmt_pids,
            pat: None,
            sdt: None,
            sections: HashMap::new(),
            last_new_present_following: None,
        }
    }

    /// Adds the sections completed by `data`. Returns whether a table is
    /// new or changed.
    pub fn push(&mut self, data: &[u8], assembler: &mut SectionAssembler, now: Instant) -> bool {
        let mut changed = false;
        for section in assembler.push_data(data) {
            if let Some(pat) = Pat::parse(&section) {
                changed |= self.pat.as_ref() != Some(&pat);
                self.pat = Some(pat);
            } else if let Some(sdt) = Sdt::parse(&section) {
                changed |= self.sdt.as_ref() != Some(&sdt);
                self.sdt = Some(sdt);
            } else if let Some(eit) = Eit::parse(&section) {
                let key = (eit.table_id, eit.service, eit.section_number);
                let is_new = self
                    .sections
                    .get(&key)
                    .is_none_or(|known| known.version != eit.version);
                if is_new {
                    if eit.is_present_following() {
                        self.last_new_present_following = Some(now);
                    }
                    self.sections.insert(key, eit);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Returns `true` if the present and following events of all wanted
    /// services of this multiplex were received.
    fn is_complete(&self) -> bool {
        let (Some(pat), Some(sdt)) = (&self.pat, &self.sdt) else {
            return false;
        };

        let mut wanted = pat
            .programs
            .iter()
            .filter(|(_, pmt_pid)| self.wanted_pmt_pids.contains(pmt_pid))
            .peekable();
        wanted.peek().is_some()
            && wanted.all(|(service_id, _)| {
                let service = ServiceKey {
                    original_network_id: sdt.original_network_id,
                    transport_stream_id: pat.transport_stream_id,
                    service_id: *service_id,
                };
                [0, 1]
                    .iter()
                    .all(|number| self.sections.contains_key(&(0x4E, service, *number)))
            })
    }

    /// Returns when the scan should end at the latest, given what was
    /// received so far.
    fn deadline(&self, start: Instant) -> Instant {
        match self.last_new_present_following {
            Some(last) => (last + QUIET_TIME).min(start + MAX_DURATION),
            None => start + MAX_DURATION,
        }
    }

    /// Returns what was collected so far, and goes on collecting.
    pub fn snapshot(&self) -> Scan {
        Scan {
            pat: self.pat.clone(),
            sdt: self.sdt.clone(),
            eit: self.sections.values().cloned().collect(),
        }
    }

    pub fn finish(self) -> Scan {
        Scan {
            pat: self.pat,
            sdt: self.sdt,
            eit: self.sections.into_values().collect(),
        }
    }
}

/// The result of a scan.
#[derive(Debug, Default)]
pub struct Scan {
    pub pat: Option<Pat>,
    pub sdt: Option<Sdt>,
    pub eit: Vec<Eit>,
}

#[derive(Debug)]
pub enum ScanError {
    /// `stop` finished first.
    Stopped,
    Failed(eyre::Report),
}

impl Scan {
    /// The `original_network_id` of the multiplex, from the SDT or from the
    /// EIT of one of its services.
    pub fn original_network_id(&self) -> Option<u16> {
        let pat = self.pat.as_ref()?;
        self.sdt
            .as_ref()
            .filter(|sdt| sdt.transport_stream_id == pat.transport_stream_id)
            .map(|sdt| sdt.original_network_id)
            .or_else(|| {
                self.eit
                    .iter()
                    .find(|eit| {
                        eit.table_id == 0x4E
                            && eit.service.transport_stream_id == pat.transport_stream_id
                    })
                    .map(|eit| eit.service.original_network_id)
            })
    }
}

impl<E: Into<eyre::Report>> From<E> for ScanError {
    fn from(e: E) -> Self {
        Self::Failed(e.into())
    }
}

/// Scans the multiplex at `url` until it is complete, a quiet period
/// started, the time ran out or `stop` finished.
///
/// The RTSP session is always torn down before this function returns.
pub async fn scan(
    url: &str,
    wanted_pmt_pids: HashSet<u16>,
    stop: impl Future<Output = ()>,
) -> Result<Scan, ScanError> {
    tokio::pin!(stop);

    let mut stream = tokio::select! {
        biased;
        _ = &mut stop => return Err(ScanError::Stopped),
        stream = Stream::open(url) => stream?,
    };

    let result = collect(&mut stream, wanted_pmt_pids, stop).await;
    stream.teardown().await;
    result
}

async fn collect(
    stream: &mut Stream,
    wanted_pmt_pids: HashSet<u16>,
    stop: impl Future<Output = ()>,
) -> Result<Scan, ScanError> {
    tokio::pin!(stop);

    let mut assembler = SectionAssembler::new(&[PID_PAT, PID_SDT, PID_EIT]);
    let mut collector = Collector::new(wanted_pmt_pids);
    let start = Instant::now();

    loop {
        let deadline = collector.deadline(start);
        let remaining = deadline.saturating_duration_since(Instant::now());

        let received = tokio::select! {
            biased;
            _ = &mut stop => return Err(ScanError::Stopped),
            received = timeout(remaining, stream.recv()) => received,
        };

        match received {
            Ok(Ok(data)) => {
                collector.push(data, &mut assembler, Instant::now());
                if collector.is_complete() {
                    break;
                }
            }
            Ok(Err(e)) => return Err(e.into()),
            // the deadline passed
            Err(_) => break,
        }
    }

    Ok(collector.finish())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        super::mock::{MockBox, MockMux, MockService},
        *,
    };

    const NOW: i64 = 1_700_000_000;

    fn mux() -> MockMux {
        MockMux {
            original_network_id: 1,
            transport_stream_id: 2,
            services: vec![
                MockService {
                    service_id: 0x4001,
                    pmt_pid: 0x100,
                    name: "Das Erste HD",
                    events: vec![("Tagesschau", NOW - 600, 900), ("Wetter", NOW + 300, 300)],
                    details: vec![],
                },
                MockService {
                    service_id: 0x4002,
                    pmt_pid: 0x200,
                    name: "ZDF HD",
                    events: vec![("heute", NOW - 100, 1000), ("Film", NOW + 900, 5400)],
                    details: vec![],
                },
            ],
            other: vec![(
                ServiceKey {
                    original_network_id: 1,
                    transport_stream_id: 3,
                    service_id: 0x5001,
                },
                MockService {
                    service_id: 0x5001,
                    pmt_pid: 0,
                    name: "",
                    events: vec![("Nachbar", NOW - 50, 100)],
                    details: vec![],
                },
            )],
            ..Default::default()
        }
    }

    async fn mock_box() -> MockBox {
        MockBox::start(HashMap::from([(450, mux())])).await
    }

    #[tokio::test]
    async fn scans_a_multiplex() {
        let server = mock_box().await;

        let scan = scan(
            &server.url(450, "0,16,17,18,20"),
            HashSet::from([0x100, 0x200]),
            std::future::pending(),
        )
        .await
        .unwrap();

        let pat = scan.pat.unwrap();
        assert_eq!(pat.transport_stream_id, 2);
        assert_eq!(pat.programs, [(0x4001, 0x100), (0x4002, 0x200)]);
        let sdt = scan.sdt.unwrap();
        assert_eq!(sdt.original_network_id, 1);
        assert_eq!(sdt.services[1], (0x4002, "ZDF HD".to_owned()));

        let mut titles: Vec<_> = scan
            .eit
            .iter()
            .flat_map(|eit| eit.events.iter().map(|e| e.title.as_str()))
            .collect();
        titles.sort();
        assert!(
            ["Film", "Tagesschau", "Wetter", "heute"]
                .iter()
                .all(|title| titles.contains(title)),
            "{titles:?}"
        );

        assert_eq!(server.setups.load(Ordering::SeqCst), 1);
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 1);
        assert_eq!(*server.requested_pids.lock().unwrap(), ["0,16,17,18,20"]);
    }

    #[tokio::test]
    async fn stops_on_request_and_tears_down() {
        let server = mock_box().await;

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn({
            let url = server.url(450, "0,16,17,18,20");
            async move {
                // a service that never has events keeps the scan running
                scan(&url, HashSet::from([0x999]), async {
                    let _ = stopped.await;
                })
                .await
            }
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.send(()).unwrap();

        assert!(matches!(task.await.unwrap(), Err(ScanError::Stopped)));
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reports_unknown_multiplexes() {
        let server = mock_box().await;

        let result = scan(
            &server.url(999, "0,16,17,18,20"),
            HashSet::new(),
            std::future::pending(),
        )
        .await;

        let Err(ScanError::Failed(e)) = result else {
            panic!("expected an error");
        };
        assert!(e.to_string().contains("404"), "{e:#}");
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn gives_up_on_unreachable_servers() {
        // nothing listens on port 1
        let result = scan(
            "rtsp://127.0.0.1:1/?freq=450",
            HashSet::new(),
            std::future::pending(),
        )
        .await;
        assert!(matches!(result, Err(ScanError::Failed(_))));
    }
}
