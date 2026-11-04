// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! The programme guide (EPG) of the DVB-C channels.
//!
//! The FRITZ!Box passes the service information of the multiplex on, which
//! includes the Event Information Table (EIT) with the programme data. Every
//! stream URL of the FRITZ!Box already requests the PIDs of the tables. As
//! the EIT of a channel is only complete on its own multiplex (cable
//! networks often send the current and next event of other multiplexes
//! as well), Televido visits the multiplexes one after another with short
//! RTSP sessions while no channel is playing, those of channels without
//! programme information first.
//!
//! The transport stream is received and parsed by the modules at the crate
//! root: [`crate::rtsp`] is a minimal RTSP client, [`crate::ts`],
//! [`crate::si`] and [`crate::text`] parse the transport stream and the
//! tables. The private submodules of this module build the guide from them:
//!
//! - `scan` reads the tables of one multiplex.
//! - `mux` groups the channels by multiplex and finds their service.
//! - `store` holds the events.
//! - `tuner` and `sweep` share the tuner with playback (see [`Tuner`]).

#[cfg(test)]
mod mock;
mod mux;
mod scan;
mod store;
mod sweep;
mod tuner;

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use tokio::sync::{mpsc::UnboundedSender, watch};

use self::{store::EpgStore, sweep::Timing};
use crate::FritzChannel;

pub use self::{
    mux::identity,
    store::{Event, NowNext},
    sweep::unix_now,
    tuner::Tuner,
};

#[derive(Debug)]
pub struct Epg {
    inner: Arc<Inner>,
    /// Dropping the sender stops the running sweep.
    cancel: Mutex<Option<watch::Sender<()>>>,
}

#[derive(Debug)]
struct Inner {
    store: Mutex<EpgStore>,
    tuner: Arc<Tuner>,
    cache_file: PathBuf,
    cache_loaded: AtomicBool,
    timing: Timing,
}

impl Epg {
    /// `cache_file` keeps the programme data between runs.
    pub fn new(cache_file: PathBuf) -> Self {
        Self::with_timing(cache_file, Timing::default())
    }

    fn with_timing(cache_file: PathBuf, timing: Timing) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: Mutex::new(EpgStore::default()),
                tuner: Arc::new(Tuner::default()),
                cache_file,
                cache_loaded: AtomicBool::new(false),
                timing,
            }),
            cancel: Mutex::new(None),
        }
    }

    pub fn tuner(&self) -> Arc<Tuner> {
        self.inner.tuner.clone()
    }

    /// Returns the running and the following event of the channel with the
    /// given URL.
    pub fn now_next(&self, url: &str, now: i64) -> Option<NowNext> {
        self.inner.store().now_next(identity(url), now)
    }

    /// Starts keeping the programme guide of `channels` up to date and
    /// stops the previous sweep. `notify` receives a message whenever
    /// events changed.
    ///
    /// Must be called within the tokio runtime.
    pub fn start(&self, channels: Vec<FritzChannel>, notify: UnboundedSender<()>) {
        let (cancel, cancelled) = watch::channel(());
        // dropping the previous sender stops the previous sweep
        *self.cancel.lock().unwrap() = Some(cancel);

        tokio::spawn(sweep::run(self.inner.clone(), channels, cancelled, notify));
    }

    /// Stops the sweep. A running scan ends its session first.
    pub fn stop(&self) {
        self.cancel.lock().unwrap().take();
    }

    /// Stops the sweep and waits (for a moment) until a running scan has
    /// released the tuner of the FRITZ!Box. Blocks the thread, for use when
    /// the application quits.
    ///
    /// Must not be called within the tokio runtime.
    pub fn shutdown(&self) {
        self.stop();
        self.inner
            .tuner
            .wait_until_released(Duration::from_millis(1500));
    }
}

impl Inner {
    fn store(&self) -> MutexGuard<'_, EpgStore> {
        self.store.lock().unwrap()
    }

    /// Loads the cached programme data once.
    async fn load_cache(&self) {
        if self.cache_loaded.swap(true, Ordering::SeqCst) {
            return;
        }
        match tokio::fs::read(&self.cache_file).await {
            Ok(json) => {
                *self.store() = EpgStore::from_json(&json, sweep::unix_now());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => tracing::warn!("failed to read the programme guide cache: {e}"),
        }
    }

    async fn save_cache(&self) {
        let json = match self.store().to_json() {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("failed to serialize the programme guide: {e}");
                return;
            }
        };

        let write = async {
            if let Some(parent) = self.cache_file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let temporary = self.cache_file.with_extension("json.tmp");
            tokio::fs::write(&temporary, json).await?;
            tokio::fs::rename(&temporary, &self.cache_file).await
        };
        if let Err(e) = write.await {
            tracing::warn!("failed to cache the programme guide: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::Ordering};

    use tokio::{sync::mpsc::unbounded_channel, time::timeout};

    use super::{
        mock::{MockBox, MockMux, MockService, ServiceInformationOnly},
        *,
    };
    use crate::si::ServiceKey;

    fn service(
        id: u16,
        pmt_pid: u16,
        name: &'static str,
        titles: [&'static str; 2],
    ) -> MockService {
        let now = unix_now();
        MockService {
            service_id: id,
            pmt_pid,
            name,
            events: vec![(titles[0], now - 600, 1200), (titles[1], now + 600, 1800)],
            details: vec![],
        }
    }

    /// Two multiplexes. The first announces one service of the second
    /// ("EIT other").
    fn muxes() -> HashMap<u32, MockMux> {
        let second_service = service(0x5001, 0x300, "Zweites", ["Nachbar", "Film"]);
        HashMap::from([
            (
                450,
                MockMux {
                    original_network_id: 1,
                    transport_stream_id: 2,
                    services: vec![
                        service(0x4001, 0x100, "Das Erste HD", ["Tagesschau", "Wetter"]),
                        service(0x4002, 0x200, "ZDF HD", ["heute", "Krimi"]),
                    ],
                    other: vec![(
                        ServiceKey {
                            original_network_id: 1,
                            transport_stream_id: 3,
                            service_id: 0x5001,
                        },
                        second_service.clone(),
                    )],
                    ..Default::default()
                },
            ),
            (
                338,
                MockMux {
                    original_network_id: 1,
                    transport_stream_id: 3,
                    services: vec![second_service],
                    other: vec![],
                    ..Default::default()
                },
            ),
        ])
    }

    fn channel(server: &MockBox, name: &str, frequency: u32, pmt_pid: u16) -> FritzChannel {
        FritzChannel {
            name: name.to_owned(),
            url: server.url(
                frequency,
                &format!("0,16,17,18,20,{pmt_pid},{},{}", pmt_pid + 1, pmt_pid + 2),
            ),
        }
    }

    fn timing() -> Timing {
        Timing {
            settle: Duration::from_millis(50),
            poll: Duration::from_millis(100),
            retry: Duration::from_secs(60),
        }
    }

    fn cache_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("televido-epg-test-{}", std::process::id()));
        dir.join(name)
    }

    async fn wait_for(
        notifications: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
        mut condition: impl FnMut() -> bool,
    ) {
        timeout(Duration::from_secs(10), async {
            while !condition() {
                notifications.recv().await.unwrap();
            }
        })
        .await
        .expect("timed out");
    }

    #[tokio::test]
    async fn collects_programme_data_of_all_multiplexes() {
        let server = MockBox::start(muxes()).await;
        let channels = vec![
            channel(&server, "Das Erste HD", 450, 256),
            channel(&server, "ZDF HD", 450, 512),
            channel(&server, "Zweites", 338, 768),
        ];

        let epg = Epg::with_timing(cache_file("collects.json"), timing());
        let (notify, mut notifications) = unbounded_channel();
        epg.start(channels.clone(), notify);

        let now = unix_now();
        wait_for(&mut notifications, || {
            channels
                .iter()
                .all(|channel| epg.now_next(&channel.url, now).is_some())
        })
        .await;

        let title = |channel: &FritzChannel| {
            let now_next = epg.now_next(&channel.url, now).unwrap();
            (
                now_next.current.map(|event| event.title),
                now_next.next.map(|event| event.title),
            )
        };
        assert_eq!(
            title(&channels[0]),
            (Some("Tagesschau".to_owned()), Some("Wetter".to_owned()))
        );
        assert_eq!(
            title(&channels[1]),
            (Some("heute".to_owned()), Some("Krimi".to_owned()))
        );
        assert_eq!(
            title(&channels[2]),
            (Some("Nachbar".to_owned()), Some("Film".to_owned()))
        );

        // the sessions of all scans were ended
        epg.stop();
        timeout(Duration::from_secs(5), async {
            while server.teardowns.load(Ordering::SeqCst) != server.setups.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(server.setups.load(Ordering::SeqCst), 2);
        // only the service information was requested
        assert!(server
            .requested_pids
            .lock()
            .unwrap()
            .iter()
            .all(|pids| pids == "0,16,17,18,20"));

        // the next run starts with the cached data and doesn't need the FRITZ!Box
        let epg = Epg::with_timing(cache_file("collects.json"), timing());
        let (notify, mut notifications) = unbounded_channel();
        epg.start(channels.clone(), notify);
        wait_for(&mut notifications, || {
            channels
                .iter()
                .all(|channel| epg.now_next(&channel.url, now).is_some())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(server.setups.load(Ordering::SeqCst), 2);
        epg.stop();
    }

    #[tokio::test]
    async fn playback_interrupts_scanning() {
        let server = MockBox::start(muxes()).await;
        // the PMT PID is not in the PAT: the scan only ends when it times out
        let channels = vec![channel(&server, "Unknown", 450, 4000)];

        let epg = Epg::with_timing(cache_file("playback.json"), timing());
        let (notify, notifications) = unbounded_channel();
        epg.start(channels, notify);

        // wait for the session
        timeout(Duration::from_secs(5), async {
            while server.setups.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let tuner = epg.tuner();
        timeout(Duration::from_secs(3), tuner.begin_playback())
            .await
            .unwrap();
        // the session was ended before playback started
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 1);

        // no new session while playing
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(server.setups.load(Ordering::SeqCst), 1);

        // scanning continues afterwards
        tuner.end_playback();
        timeout(Duration::from_secs(5), async {
            while server.setups.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        epg.stop();
        drop(notifications);
        timeout(Duration::from_secs(5), async {
            while server.teardowns.load(Ordering::SeqCst) < server.setups.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelling ends the session");
    }

    #[tokio::test]
    async fn stopping_ends_the_sweep() {
        let server = MockBox::start(muxes()).await;
        let channels = vec![channel(&server, "Das Erste HD", 450, 256)];

        let epg = Epg::with_timing(cache_file("stopping.json"), timing());
        let (cancel, cancelled) = watch::channel(());
        let (notify, _notifications) = unbounded_channel();
        let sweep = tokio::spawn(sweep::run(epg.inner.clone(), channels, cancelled, notify));

        timeout(Duration::from_secs(5), async {
            while server.setups.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        drop(cancel);
        timeout(Duration::from_secs(3), sweep)
            .await
            .expect("the sweep ends")
            .unwrap();
        assert_eq!(
            server.teardowns.load(Ordering::SeqCst),
            server.setups.load(Ordering::SeqCst)
        );
    }

    async fn falls_back_to_all_pids(behaviour: ServiceInformationOnly) {
        let mut muxes = muxes();
        muxes.get_mut(&450).unwrap().service_information_only = behaviour;
        let server = MockBox::start(muxes).await;
        let channels = vec![channel(&server, "Das Erste HD", 450, 256)];

        let epg = Epg::with_timing(
            cache_file(&format!("fallback-{behaviour:?}.json")),
            timing(),
        );
        let (notify, mut notifications) = unbounded_channel();
        epg.start(channels.clone(), notify);

        let now = unix_now();
        wait_for(&mut notifications, || {
            epg.now_next(&channels[0].url, now).is_some()
        })
        .await;
        epg.stop();

        // the first attempt requested only the service information
        let requested = server.requested_pids.lock().unwrap().clone();
        assert_eq!(requested[0], "0,16,17,18,20", "{requested:?}");
        assert_eq!(requested[1], "0,16,17,18,20,256,257,258", "{requested:?}");
        // and the full stream isn't requested again
        assert!(
            requested[2..].iter().all(|pids| pids != "0,16,17,18,20"),
            "{requested:?}"
        );
    }

    #[tokio::test]
    async fn retries_with_all_pids_if_the_stream_is_rejected() {
        falls_back_to_all_pids(ServiceInformationOnly::Rejects).await;
    }

    #[tokio::test]
    async fn retries_with_all_pids_if_the_stream_is_silent() {
        falls_back_to_all_pids(ServiceInformationOnly::SendsNothing).await;
    }
}
