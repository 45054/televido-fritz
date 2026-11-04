// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Groups the channels of the FRITZ!Box by multiplex and matches them with
//! the services announced in the DVB tables.
//!
//! The URL of a channel looks like
//! `rtsp://192.168.178.1:554/?avm=1&freq=450&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20,6100,6110,6120`.
//! The FRITZ!Box adds the PIDs of the service information (0, 16, 17, 18 and
//! 20) and the PIDs of the channel: its PMT, video, audio and subtitles. The
//! PMT PID is also listed in the PAT of the multiplex, which identifies the
//! service of the channel without relying on names.

use crate::{
    si::{Pat, Sdt, ServiceKey},
    FritzChannel,
};

/// The PIDs of the service information requested to read the programme
/// guide. These are the same the FRITZ!Box adds to every channel.
pub const SERVICE_INFORMATION_PIDS: [u16; 5] = [0, 16, 17, 18, 20];

/// All channels on one frequency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mux {
    /// Identifies the multiplex, e.g. `450/dvbc/256qam/6900`.
    pub key: String,
    /// URL that only requests the service information.
    pub scan_url: String,
    /// URL of the first channel, which requests the audio and video, too.
    ///
    /// Used if the FRITZ!Box doesn't accept the stream without video.
    pub fallback_url: String,
    pub channels: Vec<MuxChannel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuxChannel {
    /// See [`identity`].
    pub identity: String,
    pub name: String,
    /// The PIDs of the channel, without the PIDs of the service information.
    pub pids: Vec<u16>,
}

impl Mux {
    /// The PIDs of the channels, which contain their PMT PIDs.
    pub fn channel_pids(&self) -> impl Iterator<Item = u16> + '_ {
        self.channels
            .iter()
            .flat_map(|channel| channel.pids.iter().copied())
    }
}

/// Returns the part of the channel URL that identifies the channel. It
/// doesn't contain the address of the FRITZ!Box.
pub fn identity(url: &str) -> &str {
    url.split_once('?').map_or(url, |(_, query)| query)
}

/// Groups `channels` by multiplex in the order of their first appearance.
/// Channels without frequency are left out.
pub fn group(channels: &[FritzChannel]) -> Vec<Mux> {
    let mut muxes: Vec<Mux> = Vec::new();

    for channel in channels {
        let Some((base, query)) = channel.url.split_once('?') else {
            continue;
        };
        let parameters: Vec<(&str, &str)> = query
            .split('&')
            .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
            .collect();
        let get = |name: &str| {
            parameters
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| *value)
        };

        let Some(frequency) = get("freq").filter(|frequency| !frequency.is_empty()) else {
            continue;
        };
        let key = format!(
            "{frequency}/{}/{}/{}",
            get("msys").unwrap_or_default(),
            get("mtype").unwrap_or_default(),
            get("sr").unwrap_or_default()
        );

        let pids: Vec<u16> = get("pids")
            .unwrap_or_default()
            .split(',')
            .filter_map(|pid| pid.trim().parse().ok())
            .filter(|pid| !SERVICE_INFORMATION_PIDS.contains(pid))
            .collect();
        let channel = MuxChannel {
            identity: identity(&channel.url).to_owned(),
            name: channel.name.clone(),
            pids,
        };

        match muxes.iter_mut().find(|mux| mux.key == key) {
            Some(mux) => mux.channels.push(channel),
            None => {
                let pids = SERVICE_INFORMATION_PIDS
                    .map(|pid| pid.to_string())
                    .join(",");
                let scan_query: Vec<String> = parameters
                    .iter()
                    .filter(|(key, _)| *key != "pids")
                    .map(|(key, value)| format!("{key}={value}"))
                    .chain([format!("pids={pids}")])
                    .collect();

                muxes.push(Mux {
                    key,
                    scan_url: format!("{base}?{}", scan_query.join("&")),
                    fallback_url: channel_url(base, query),
                    channels: vec![channel],
                });
            }
        }
    }

    muxes
}

fn channel_url(base: &str, query: &str) -> String {
    format!("{base}?{query}")
}

/// Matches the channels of `mux` with the services of a scan.
///
/// Returns `(identity, service)` for every channel whose PMT PID is in the
/// PAT. If several services use the same PMT PID, the name decides.
pub fn match_services(
    mux: &Mux,
    original_network_id: u16,
    pat: &Pat,
    sdt: Option<&Sdt>,
) -> Vec<(String, ServiceKey)> {
    mux.channels
        .iter()
        .filter_map(|channel| {
            let candidates: Vec<u16> = pat
                .programs
                .iter()
                .filter(|(_, pmt_pid)| channel.pids.contains(pmt_pid))
                .map(|(service_id, _)| *service_id)
                .collect();

            let service_id = match candidates.as_slice() {
                [] => return None,
                [service_id] => *service_id,
                candidates => candidates
                    .iter()
                    .copied()
                    .find(|service_id| {
                        sdt.is_some_and(|sdt| {
                            sdt.services
                                .iter()
                                .any(|(id, name)| id == service_id && *name == channel.name)
                        })
                    })
                    .unwrap_or(candidates[0]),
            };

            Some((
                channel.identity.clone(),
                ServiceKey {
                    original_network_id,
                    transport_stream_id: pat.transport_stream_id,
                    service_id,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(name: &str, url: &str) -> FritzChannel {
        FritzChannel {
            name: name.to_owned(),
            url: url.to_owned(),
        }
    }

    const ZDF: &str = "rtsp://192.168.178.1:554/?avm=1&freq=450&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20,6100,6110,6120,6121,6123,6130,6131,6170";
    const ZDF_NEO: &str = "rtsp://192.168.178.1:554/?avm=1&freq=450&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20,6200,6210,6220";
    const BR: &str = "rtsp://192.168.178.1:554/?avm=1&freq=338&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20,5200,5201,1076,1570";

    #[test]
    fn groups_channels_by_frequency() {
        let muxes = group(&[
            channel("ZDF HD", ZDF),
            channel("BR Fernsehen Süd HD", BR),
            channel("zdf_neo HD", ZDF_NEO),
            channel("Broken", "rtsp://192.168.178.1:554/?avm=1&pids=0"),
            channel("No query", "rtsp://192.168.178.1:554/"),
        ]);

        assert_eq!(muxes.len(), 2);
        assert_eq!(muxes[0].key, "450/dvbc/256qam/6900");
        assert_eq!(muxes[1].key, "338/dvbc/256qam/6900");
        assert_eq!(
            muxes[0]
                .channels
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["ZDF HD", "zdf_neo HD"]
        );
        assert_eq!(
            muxes[0].channels[0].pids,
            [6100, 6110, 6120, 6121, 6123, 6130, 6131, 6170]
        );
        assert_eq!(
            muxes[0].scan_url,
            "rtsp://192.168.178.1:554/?avm=1&freq=450&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20"
        );
        assert_eq!(muxes[0].fallback_url, ZDF);
        assert_eq!(
            muxes[0].channels[0].identity,
            "avm=1&freq=450&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids=0,16,17,18,20,6100,6110,6120,6121,6123,6130,6131,6170"
        );
        assert_eq!(
            muxes[1].channel_pids().collect::<Vec<_>>(),
            [5200, 5201, 1076, 1570]
        );
    }

    #[test]
    fn identity_ignores_the_address() {
        assert_eq!(
            identity("rtsp://192.168.178.1:554/?avm=1&freq=450"),
            identity("rtsp://fritz.box:554/?avm=1&freq=450")
        );
        assert_eq!(identity("no-query"), "no-query");
    }

    #[test]
    fn matches_services_by_pmt_pid() {
        let mux = &group(&[channel("ZDF HD", ZDF), channel("zdf_neo HD", ZDF_NEO)])[0];
        let pat = Pat {
            transport_stream_id: 0x0102,
            programs: vec![(0x2B8C, 6200), (0x2B8B, 6100), (0x2B8D, 7000)],
        };
        let sdt = Sdt {
            original_network_id: 0x0085,
            transport_stream_id: 0x0102,
            services: vec![],
        };

        let matches = match_services(mux, 0x0085, &pat, Some(&sdt));
        let key = |service_id| ServiceKey {
            original_network_id: 0x0085,
            transport_stream_id: 0x0102,
            service_id,
        };
        assert_eq!(
            matches,
            [
                (mux.channels[0].identity.clone(), key(0x2B8B)),
                (mux.channels[1].identity.clone(), key(0x2B8C)),
            ]
        );
    }

    #[test]
    fn uses_names_for_shared_pmt_pids() {
        let mux = &group(&[channel("Second", ZDF)])[0];
        let pat = Pat {
            transport_stream_id: 1,
            programs: vec![(10, 6100), (11, 6100)],
        };
        let sdt = Sdt {
            original_network_id: 2,
            transport_stream_id: 1,
            services: vec![(10, "First".to_owned()), (11, "Second".to_owned())],
        };

        assert_eq!(match_services(mux, 2, &pat, Some(&sdt))[0].1.service_id, 11);
        // without a matching name the first service is used
        assert_eq!(match_services(mux, 2, &pat, None)[0].1.service_id, 10);
    }

    #[test]
    fn leaves_unknown_channels_unmatched() {
        let mux = &group(&[channel("ZDF HD", ZDF)])[0];
        let pat = Pat {
            transport_stream_id: 1,
            programs: vec![(10, 7000)],
        };
        assert!(match_services(mux, 2, &pat, None).is_empty());
    }
}
