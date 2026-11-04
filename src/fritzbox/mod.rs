// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! DVB-C channels of a FRITZ!Box Cable.
//!
//! The FRITZ!Box exposes the result of its own channel scan as M3U playlists
//! at `http://<address>/dvb/m3u/{tv,tvhd,tvsd,radio}.m3u`. `tv.m3u` lists
//! every TV channel once in the best available quality; older FRITZ!OS
//! versions only provide the separate HD and SD lists. Every entry is an
//! `rtsp://` URL which is played with mpv.
//!
//! This module holds the GTK side: loading the lists over HTTP, the channel
//! logos and the view. The parts without GTK live in the workspace crate
//! `televido-dvb`: the playlist parser ([`televido_dvb::m3u`]), the RTSP
//! client and the transport stream parsers, and the programme guide, which
//! is read from the DVB stream itself ([`televido_dvb::epg`]). [`playback`]
//! plays a channel through the relay of `televido-dvb`.

mod logos;
pub mod playback;
mod tvapp;
mod view;

use std::time::Duration;

use eyre::WrapErr;
use gettextrs::gettext;
use reqwest::{StatusCode, Url};
use televido_dvb::m3u;

use crate::config::{APP_ID, PROJECT_URL, VERSION};

pub use self::{
    logos::{LogoIndex, LogoSources},
    view::TvFritzView,
};
pub use televido_dvb::{unix_now, Epg, FritzChannel, NowNext};

pub const DEFAULT_ADDRESS: &str = "fritz.box";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelList {
    Tv,
    Hd,
    Sd,
    Radio,
}

impl ChannelList {
    /// Returns the lists to load, in display order.
    pub fn selected(separate_hd_sd: bool, show_radio: bool) -> Vec<ChannelList> {
        let mut lists = if separate_hd_sd {
            vec![ChannelList::Hd, ChannelList::Sd]
        } else {
            vec![ChannelList::Tv]
        };
        if show_radio {
            lists.push(ChannelList::Radio);
        }
        lists
    }

    fn path(self) -> &'static str {
        match self {
            ChannelList::Tv => "dvb/m3u/tv.m3u",
            ChannelList::Hd => "dvb/m3u/tvhd.m3u",
            ChannelList::Sd => "dvb/m3u/tvsd.m3u",
            ChannelList::Radio => "dvb/m3u/radio.m3u",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FritzChannels {
    pub tv: Vec<FritzChannel>,
    pub hd: Vec<FritzChannel>,
    pub sd: Vec<FritzChannel>,
    pub radio: Vec<FritzChannel>,
}

impl FritzChannels {
    pub fn get(&self, list: ChannelList) -> &[FritzChannel] {
        match list {
            ChannelList::Tv => &self.tv,
            ChannelList::Hd => &self.hd,
            ChannelList::Sd => &self.sd,
            ChannelList::Radio => &self.radio,
        }
    }
    fn get_mut(&mut self, list: ChannelList) -> &mut Vec<FritzChannel> {
        match list {
            ChannelList::Tv => &mut self.tv,
            ChannelList::Hd => &mut self.hd,
            ChannelList::Sd => &mut self.sd,
            ChannelList::Radio => &mut self.radio,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.tv.is_empty() && self.hd.is_empty() && self.sd.is_empty() && self.radio.is_empty()
    }
}

#[derive(Debug)]
pub struct FritzBox {
    http: reqwest::Client,
    epg: Epg,
}

impl FritzBox {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(format!("{APP_ID}/{VERSION} ({PROJECT_URL})"))
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()?,
            epg: Epg::new(adw::glib::user_cache_dir().join("televido/fritzbox-epg.json")),
        })
    }

    /// The programme guide of the channels.
    pub fn epg(&self) -> &Epg {
        &self.epg
    }

    pub async fn channels(
        &self,
        address: &str,
        lists: &[ChannelList],
    ) -> eyre::Result<FritzChannels> {
        let base_url = base_url(address)?;
        let mut channels = FritzChannels::default();

        for &list in lists {
            match self.channel_list(&base_url, list).await? {
                Some(entries) => *channels.get_mut(list) = entries,
                // older FRITZ!OS versions only provide the separate lists
                None if list == ChannelList::Tv => {
                    for list in [ChannelList::Hd, ChannelList::Sd] {
                        *channels.get_mut(list) = self
                            .channel_list(&base_url, list)
                            .await?
                            .unwrap_or_default();
                    }
                }
                // a missing list (e.g. no radio channels) is not an error
                None => (),
            }
        }

        Ok(channels)
    }

    /// Returns `None` if the FRITZ!Box doesn't provide the list.
    async fn channel_list(
        &self,
        base_url: &Url,
        list: ChannelList,
    ) -> eyre::Result<Option<Vec<FritzChannel>>> {
        let url = base_url.join(list.path())?;

        let response = self.http.get(url.clone()).send().await.wrap_err_with(|| {
            // translators: `{}` is replaced by the address, e.g. `fritz.box`
            gettext("Could not connect to the FRITZ!Box at “{}”")
                .replace("{}", base_url.authority())
        })?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let bytes = response.error_for_status()?.bytes().await?;
        let text = m3u::decode(&bytes);

        if !m3u::is_playlist(&text) {
            eyre::bail!(
                // translators: `{}` is replaced by a URL
                gettext("“{}” is not a channel list").replace("{}", url.as_str())
            );
        }

        Ok(Some(m3u::parse(&text)))
    }

    pub async fn logo_index(&self) -> LogoIndex {
        logos::load_index(self.http.clone()).await
    }

    pub async fn logo(&self, sources: LogoSources) -> eyre::Result<Vec<u8>> {
        logos::load_logo(self.http.clone(), sources).await
    }
}

/// Builds the base URL from a user-provided address such as `fritz.box`,
/// `192.168.178.1`, `192.168.178.1:8080` or `http://fritz.box/`.
pub fn base_url(address: &str) -> eyre::Result<Url> {
    let address = address.trim();
    let address = address
        .strip_prefix("http://")
        .or_else(|| address.strip_prefix("https://"))
        .unwrap_or(address);
    let host = address.split('/').next().unwrap_or_default();
    let host = if host.is_empty() {
        DEFAULT_ADDRESS
    } else {
        host
    };

    Url::parse(&format!("http://{host}/"))
        .ok()
        .filter(|url| url.host_str().is_some_and(|host| !host.is_empty()))
        .ok_or_else(|| {
            eyre::eyre!(
                // translators: `{}` is replaced by the address entered by the user
                gettext("“{}” is not a valid address").replace("{}", address)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_base_url() {
        for (address, expected) in [
            ("fritz.box", "http://fritz.box/"),
            ("  192.168.178.1 ", "http://192.168.178.1/"),
            ("192.168.178.1:8080", "http://192.168.178.1:8080/"),
            ("http://fritz.box/", "http://fritz.box/"),
            ("https://fritz.box/dvb/m3u/tvhd.m3u", "http://fritz.box/"),
            ("[fd00::1]", "http://[fd00::1]/"),
            ("", "http://fritz.box/"),
        ] {
            assert_eq!(base_url(address).unwrap().as_str(), expected, "{address}");
        }

        assert_eq!(
            base_url("fritz.box")
                .unwrap()
                .join(ChannelList::Tv.path())
                .unwrap()
                .as_str(),
            "http://fritz.box/dvb/m3u/tv.m3u"
        );
    }

    #[test]
    fn selects_channel_lists() {
        assert_eq!(
            ChannelList::selected(false, true),
            [ChannelList::Tv, ChannelList::Radio]
        );
        assert_eq!(ChannelList::selected(false, false), [ChannelList::Tv]);
        assert_eq!(
            ChannelList::selected(true, true),
            [ChannelList::Hd, ChannelList::Sd, ChannelList::Radio]
        );
        assert_eq!(
            ChannelList::selected(true, false),
            [ChannelList::Hd, ChannelList::Sd]
        );
    }

    #[test]
    fn rejects_invalid_addresses() {
        assert!(base_url("fritz box").is_err());
        assert!(base_url("192.168.178.1:99999").is_err());
    }
}
