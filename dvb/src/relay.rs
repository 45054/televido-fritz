// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Receives the transport stream of a channel once and hands it to several
//! consumers: the player, recordings and the live programme guide.
//!
//! The FRITZ!Box delivers only one stream per device, so a [`Relay`] is the
//! single owner of the RTSP session while a channel plays. It announces the
//! playback to the [`Tuner`] (which pauses the programme guide sweep), and
//! distributes every RTP payload as a [`Chunk`] over a
//! [`tokio::sync::broadcast`] channel.
//!
//! - **Drop-oldest:** the receive loop never waits for a consumer. The
//!   channel buffers [`CAPACITY`] chunks; a consumer that falls further
//!   behind gets [`broadcast::error::RecvError::Lagged`], loses the oldest
//!   chunks and simply continues. That is what a player wants (like UDP
//!   overflowing before), and a recording logs the gap.
//! - **Keepalive:** the FRITZ!Box ends a session after the `timeout` of its
//!   `Session` header without a request. The relay sends
//!   [`rtsp::Control::keepalive`] at half that interval, concurrently with
//!   receiving, so that a slow answer doesn't let the socket overflow.
//! - **Stall detection:** if no packet arrives for [`STALL_TIMEOUT`] (e.g.
//!   the FRITZ!Box dropped the session or lost the signal), the relay ends
//!   with an error.
//! - **Lifecycle:** the relay ends on [`Relay::stop`], when the last handle
//!   is dropped, or on an error. In every case it closes the broadcast
//!   channel (consumers see [`broadcast::error::RecvError::Closed`]), tears
//!   down the session, ends the playback at the tuner exactly once and then
//!   resolves [`Relay::finished`].

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{
    sync::{broadcast, watch},
    time::{sleep, timeout},
};

use crate::{epg::identity, rtsp, task::stopped, Tuner};

/// One RTP payload: usually 7 transport stream packets of 188 bytes, always
/// a multiple of 188 bytes. Cloning it is cheap.
pub type Chunk = Arc<[u8]>;

/// Chunks buffered for every consumer. With 7 packets per chunk, 8192
/// chunks are about 10 MiB, several seconds of an HD channel (~2 MB/s).
pub const CAPACITY: usize = 8192;

/// The relay ends if the FRITZ!Box sends nothing for this long.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a relay ended with an error. Cheap to clone, as every handle can ask.
pub type RelayError = Arc<eyre::Report>;

/// A running relay of one channel. Cheap to clone; all clones control the
/// same relay, which stops when the last clone is dropped.
#[derive(Clone)]
pub struct Relay {
    handle: Arc<Handle>,
}

/// Owned by the handles: dropping it stops the receive loop.
struct Handle {
    stop: watch::Sender<bool>,
    shared: Arc<Shared>,
}

/// Shared by the handles and the receive loop.
struct Shared {
    url: String,
    /// Taken by the receive loop when it ends, which closes the channel.
    sender: Mutex<Option<broadcast::Sender<Chunk>>>,
    /// `None` while running.
    outcome: watch::Sender<Option<Result<(), RelayError>>>,
    chunks: AtomicU64,
    bytes: AtomicU64,
}

/// Counters of a relay, for the logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    /// Chunks (RTP payloads) received.
    pub chunks: u64,
    /// Transport stream bytes received.
    pub bytes: u64,
    /// Current number of consumers.
    pub receivers: usize,
}

/// Settings of a relay, which the tests shorten.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub capacity: usize,
    pub stall_timeout: Duration,
    pub keepalive: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capacity: CAPACITY,
            stall_timeout: STALL_TIMEOUT,
            keepalive: true,
        }
    }
}

/// Ends the playback at the tuner when dropped, so that it happens exactly
/// once, however the relay ends (even if `Relay::open` is cancelled).
struct Playback {
    tuner: Arc<Tuner>,
    generation: u64,
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.tuner.end_playback_of(self.generation);
    }
}

impl Relay {
    /// Takes the tuner from the programme guide (see
    /// [`Tuner::begin_playback`]), opens the RTSP session of `url` and starts
    /// receiving.
    ///
    /// Errors, e.g. `SETUP failed: 503 Service Unavailable` if the FRITZ!Box
    /// has no free tuner, are returned before any consumer exists, and the
    /// tuner is released again. Must be called within the tokio runtime.
    pub async fn open(url: &str, tuner: Arc<Tuner>) -> eyre::Result<Self> {
        Self::open_with(url, tuner, Settings::default()).await
    }

    pub(crate) async fn open_with(
        url: &str,
        tuner: Arc<Tuner>,
        settings: Settings,
    ) -> eyre::Result<Self> {
        let generation = tuner.begin_playback_generation().await;
        let playback = Playback { tuner, generation };

        // dropping `playback` on errors releases the tuner
        let stream = rtsp::Stream::open(url).await?;
        tracing::debug!("relaying {url}");

        let (sender, _) = broadcast::channel(settings.capacity);
        let shared = Arc::new(Shared {
            url: url.to_owned(),
            sender: Mutex::new(Some(sender.clone())),
            outcome: watch::Sender::new(None),
            chunks: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let (stop, stopped) = watch::channel(false);

        tokio::spawn(run(
            stream,
            sender,
            shared.clone(),
            stopped,
            playback,
            settings,
        ));

        Ok(Self {
            handle: Arc::new(Handle { stop, shared }),
        })
    }

    /// Returns a new consumer, which receives the chunks from now on.
    ///
    /// After the relay ended, the consumer is closed right away.
    pub fn subscribe(&self) -> broadcast::Receiver<Chunk> {
        match &*self.handle.shared.sender.lock().unwrap() {
            Some(sender) => sender.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Stops the relay. Returns immediately; [`Relay::finished`] resolves
    /// when the session was torn down.
    pub fn stop(&self) {
        self.handle.stop.send_replace(true);
    }

    /// Resolves when the relay has ended, its session was torn down and the
    /// tuner released: `Ok` if it was stopped, the error otherwise.
    pub async fn finished(&self) -> Result<(), RelayError> {
        let mut outcome = self.handle.shared.outcome.subscribe();
        let outcome = outcome
            .wait_for(Option::is_some)
            .await
            .expect("the sender lives in the handle");
        outcome.clone().expect("checked by wait_for")
    }

    /// Returns `false` once the relay has ended (or is ending).
    pub fn is_running(&self) -> bool {
        self.handle.shared.outcome.borrow().is_none()
            && self.handle.shared.sender.lock().unwrap().is_some()
    }

    /// The URL of the channel.
    pub fn url(&self) -> &str {
        &self.handle.shared.url
    }

    /// The identity of the channel, see [`identity`].
    pub fn identity(&self) -> &str {
        identity(&self.handle.shared.url)
    }

    /// Returns the counters of the relay, for the logs.
    pub fn stats(&self) -> RelayStats {
        let shared = &self.handle.shared;
        RelayStats {
            chunks: shared.chunks.load(Ordering::Relaxed),
            bytes: shared.bytes.load(Ordering::Relaxed),
            receivers: shared
                .sender
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, broadcast::Sender::receiver_count),
        }
    }
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay")
            .field("url", &self.url())
            .field("running", &self.is_running())
            .field("stats", &self.stats())
            .finish()
    }
}

async fn run(
    mut stream: rtsp::Stream,
    sender: broadcast::Sender<Chunk>,
    shared: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
    playback: Playback,
    settings: Settings,
) {
    let result = {
        let (control, receiver) = stream.parts();
        let interval = control.keepalive_interval();

        let receive = async {
            loop {
                let data = match timeout(settings.stall_timeout, receiver.recv()).await {
                    Ok(Ok(data)) => data,
                    Ok(Err(e)) => return Err(e.wrap_err("failed to receive the stream")),
                    Err(_) => {
                        return Err(eyre::eyre!(
                            "no data from the FRITZ!Box for {} s",
                            settings.stall_timeout.as_secs_f32()
                        ))
                    }
                };
                shared.chunks.fetch_add(1, Ordering::Relaxed);
                shared.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                // fails only without consumers, which may subscribe later
                let _ = sender.send(Chunk::from(data));
            }
        };

        let keepalive = async {
            if !settings.keepalive {
                return std::future::pending().await;
            }
            loop {
                sleep(interval).await;
                // a lost session shows up as a stall
                if let Err(e) = control.keepalive().await {
                    tracing::warn!("RTSP keepalive failed: {e:#}");
                }
            }
        };

        tokio::select! {
            result = receive => result,
            () = keepalive => unreachable!("the keepalive loop runs forever"),
            () = stopped(&mut stop) => Ok(()),
        }
    };

    // close the channel, so that the consumers finish their work
    shared.sender.lock().unwrap().take();
    drop(sender);

    stream.teardown().await;
    drop(playback);

    let stats = RelayStats {
        chunks: shared.chunks.load(Ordering::Relaxed),
        bytes: shared.bytes.load(Ordering::Relaxed),
        receivers: 0,
    };
    match &result {
        Ok(()) => tracing::debug!("relay of {} stopped, {stats:?}", shared.url),
        Err(e) => tracing::warn!("relay of {} failed: {e:#}, {stats:?}", shared.url),
    }
    shared.outcome.send_replace(Some(result.map_err(Arc::new)));
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::Ordering};

    use tokio::sync::broadcast::error::RecvError;

    use super::*;
    use crate::{
        epg::mock::{MockBox, MockMux, MockOptions, MockService},
        ts::PACKET_SIZE,
    };

    const WAIT: Duration = Duration::from_secs(5);

    fn muxes() -> HashMap<u32, MockMux> {
        HashMap::from([(
            450,
            MockMux {
                original_network_id: 1,
                transport_stream_id: 2,
                services: vec![MockService {
                    service_id: 0x4001,
                    pmt_pid: 0x100,
                    name: "Das Erste HD",
                    events: vec![("Tagesschau", crate::unix_now() - 600, 1200)],
                    details: vec![],
                }],
                ..Default::default()
            },
        )])
    }

    async fn server(options: MockOptions) -> MockBox {
        MockBox::start_with(muxes(), options).await
    }

    fn channel_url(server: &MockBox) -> String {
        server.url(450, "0,16,17,18,20,256,257,258")
    }

    async fn recv(receiver: &mut broadcast::Receiver<Chunk>) -> Chunk {
        timeout(WAIT, receiver.recv())
            .await
            .expect("timed out")
            .expect("a chunk")
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        timeout(WAIT, async {
            while !condition() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out");
    }

    #[tokio::test]
    async fn distributes_the_stream_until_stopped() {
        let server = server(MockOptions {
            payload_packets: 14,
            ..Default::default()
        })
        .await;
        let tuner = Arc::new(Tuner::default());
        let url = channel_url(&server);

        let relay = Relay::open(&url, tuner.clone()).await.unwrap();
        assert!(tuner.is_playing());
        assert!(relay.is_running());
        assert_eq!(relay.url(), url);
        assert!(relay.identity().starts_with("avm=1&freq=450"));

        let mut first = relay.subscribe();
        let mut second = relay.subscribe();
        for _ in 0..20 {
            let a = recv(&mut first).await;
            let b = recv(&mut second).await;
            assert_eq!(a, b, "every consumer gets the same chunks");
            assert!(!a.is_empty() && a.len().is_multiple_of(PACKET_SIZE));
            assert_eq!(a[0], 0x47);
        }
        let stats = relay.stats();
        assert!(stats.chunks >= 20 && stats.bytes >= 20 * PACKET_SIZE as u64);
        assert_eq!(stats.receivers, 2);

        relay.stop();
        timeout(WAIT, relay.finished())
            .await
            .unwrap()
            .expect("stopping is no error");
        assert!(!relay.is_running());
        assert!(!tuner.is_playing());
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 1);

        // the consumers see the end after the buffered chunks
        let closed = timeout(WAIT, async {
            loop {
                if let Err(RecvError::Closed) = first.recv().await {
                    break;
                }
            }
        });
        closed.await.unwrap();
        assert!(matches!(
            relay.subscribe().recv().await,
            Err(RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn dropping_the_last_handle_stops_the_relay() {
        let server = server(MockOptions::default()).await;
        let tuner = Arc::new(Tuner::default());

        let relay = Relay::open(&channel_url(&server), tuner.clone())
            .await
            .unwrap();
        let mut receiver = relay.subscribe();
        recv(&mut receiver).await;

        let clone = relay.clone();
        drop(relay);
        sleep(Duration::from_millis(100)).await;
        assert!(clone.is_running(), "a clone keeps the relay running");

        drop(clone);
        wait_until(|| !tuner.is_playing()).await;
        wait_until(|| server.teardowns.load(Ordering::SeqCst) == 1).await;
    }

    #[tokio::test]
    async fn keeps_the_session_alive() {
        // the mock stops streaming 1 s after the last request
        let server = server(MockOptions {
            session_timeout: Duration::from_secs(1),
            ..Default::default()
        })
        .await;
        let tuner = Arc::new(Tuner::default());

        let relay = Relay::open(&channel_url(&server), tuner.clone())
            .await
            .unwrap();
        // without keepalives, the session would have expired twice over
        sleep(Duration::from_millis(2600)).await;

        // still streaming
        recv(&mut relay.subscribe()).await;
        assert!(relay.is_running());
        assert_eq!(server.expired.load(Ordering::SeqCst), 0);
        // one every 500 ms, but a loaded machine may delay them, so only
        // check that they keep coming; the session not expiring is the point
        let keepalives = server.keepalives.load(Ordering::SeqCst);
        assert!(keepalives >= 2, "{keepalives} keepalives");

        relay.stop();
        relay.finished().await.unwrap();
    }

    #[tokio::test]
    async fn fails_if_the_stream_stalls() {
        let server = server(MockOptions {
            stop_after: Some(Duration::from_millis(300)),
            ..Default::default()
        })
        .await;
        let tuner = Arc::new(Tuner::default());

        let relay = Relay::open_with(
            &channel_url(&server),
            tuner.clone(),
            Settings {
                stall_timeout: Duration::from_millis(700),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut receiver = relay.subscribe();
        recv(&mut receiver).await;

        let error = timeout(WAIT, relay.finished())
            .await
            .unwrap()
            .expect_err("the relay fails");
        assert!(
            error.to_string().contains("no data from the FRITZ!Box"),
            "{error:#}"
        );
        assert!(!tuner.is_playing());
        assert_eq!(server.teardowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_session_times_out_without_keepalive() {
        // checks the mock, which the keepalive test relies on
        let server = server(MockOptions {
            session_timeout: Duration::from_secs(1),
            ..Default::default()
        })
        .await;
        let relay = Relay::open_with(
            &channel_url(&server),
            Arc::new(Tuner::default()),
            Settings {
                stall_timeout: Duration::from_millis(700),
                keepalive: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(timeout(WAIT, relay.finished()).await.unwrap().is_err());
        assert_eq!(server.expired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reports_errors_when_opening() {
        let tuner = Arc::new(Tuner::default());

        // nothing listens on the port
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("rtsp://127.0.0.1:{port}/?freq=450&pids=0,256");
        assert!(Relay::open(&url, tuner.clone()).await.is_err());
        assert!(!tuner.is_playing(), "the tuner is released");

        // an unknown multiplex
        let server = server(MockOptions::default()).await;
        let error = Relay::open(&server.url(999, "0,256"), tuner.clone())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "DESCRIBE failed: 404 Not Found");
        assert!(error.downcast_ref::<rtsp::StatusError>().is_some());
        assert!(!tuner.is_playing());
    }

    #[tokio::test]
    async fn a_lagging_consumer_continues() {
        let server = server(MockOptions {
            payload_packets: 70,
            ..Default::default()
        })
        .await;
        let relay = Relay::open_with(
            &channel_url(&server),
            Arc::new(Tuner::default()),
            Settings {
                capacity: 16,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut slow = relay.subscribe();
        let mut fast = relay.subscribe();
        // the fast consumer is unaffected
        for _ in 0..64 {
            recv(&mut fast).await;
        }
        assert!(matches!(slow.recv().await, Err(RecvError::Lagged(_))));
        // the newest chunks are still there
        let chunk = recv(&mut slow).await;
        assert_eq!(chunk[0], 0x47);

        relay.stop();
        relay.finished().await.unwrap();
    }

    #[tokio::test]
    async fn a_replaced_relay_does_not_end_the_new_playback() {
        let server = server(MockOptions::default()).await;
        let tuner = Arc::new(Tuner::default());
        let url = channel_url(&server);

        let old = Relay::open(&url, tuner.clone()).await.unwrap();
        // the old one ends after the new one started
        old.stop();
        let new = Relay::open(&url, tuner.clone()).await.unwrap();
        old.finished().await.unwrap();
        assert!(tuner.is_playing());

        new.stop();
        new.finished().await.unwrap();
        assert!(!tuner.is_playing());
    }
}
