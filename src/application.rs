// SPDX-FileCopyrightText: David Cabot <d-k-bo@mailbox.org>
// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{OnceCell, RefCell},
    rc::Rc,
    sync::Arc,
};

use adw::{gio, glib, prelude::*, subclass::prelude::*};
use eyre::WrapErr;
use gettextrs::gettext;
use smart_default::SmartDefault;

use crate::{
    config::{APP_ID, PROFILE, VERSION},
    fritzbox::{playback, FritzBox, TvRecordingState},
    launcher::{ExternalProgram, ExternalProgramType, ProgramSelector},
    mpv,
    preferences::TvPreferencesDialog,
    settings::{TvSettings, VideoQuality},
    utils::{show_error, spawn, spawn_clone, tokio, AsyncResource},
    window::TvWindow,
    zapp::Zapp,
};

mod imp {

    use super::*;

    #[derive(Debug, SmartDefault)]
    pub struct TvApplication {
        #[default(Arc::new(Zapp::new().expect("failed to initialize Zapp client")))]
        pub(super) zapp: Arc<Zapp>,
        #[default(Arc::new(FritzBox::new().expect("failed to initialize FRITZ!Box client")))]
        pub(super) fritzbox: Arc<FritzBox>,
        pub(super) live_channels: AsyncResource<LiveChannels>,
        pub(super) window: RefCell<Option<glib::WeakRef<TvWindow>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TvApplication {
        const NAME: &'static str = "TvApplication";
        type Type = super::TvApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for TvApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_gactions();
        }
    }

    impl ApplicationImpl for TvApplication {
        fn activate(&self) {
            self.obj().window().present();
        }

        fn shutdown(&self) {
            // first, so that the TEARDOWN of the relay reaches the FRITZ!Box
            playback::shutdown();
            self.fritzbox.epg().shutdown();
            mpv::terminate();
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for TvApplication {}
    impl AdwApplicationImpl for TvApplication {}
}

glib::wrapper! {
    pub struct TvApplication(ObjectSubclass<imp::TvApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

thread_local! {
    static APPLICATION: OnceCell<TvApplication> = const { OnceCell::new() };
}

impl TvApplication {
    pub fn new() -> Self {
        let slf: Self = glib::Object::builder()
            .property("application-id", APP_ID)
            .property("resource-base-path", "/io/github/_45054/TelevidoFritz")
            .build();

        APPLICATION.with(|app| {
            app.set(slf.clone())
                .expect("TvApplication may only be created once")
        });

        TvSettings::get().migrate_legacy();

        let live_channels = slf.live_channels();
        live_channels.set_load_fn({
            let zapp = slf.zapp();
            move || {
                let zapp = zapp.clone();
                Box::pin(async move {
                    tokio(async move { zapp.channel_info_list().await })
                        .await
                        .map(Rc::new)
                        .map_err(|e| {
                            let e = e.wrap_err(gettext("Failed to load livestream channels"));
                            tracing::error!("{e:?}");
                            Rc::new(e)
                        })
                })
            }
        });
        spawn(async move { live_channels.load() });

        slf
    }
    pub fn get() -> Self {
        APPLICATION.with(|app| {
            app.get()
                .expect("TvApplication::get() may only be called from the main thread")
                .clone()
        })
    }

    /// The connection to the session bus, established on first use.
    pub async fn dbus() -> eyre::Result<zbus::Connection> {
        static DBUS: tokio::sync::OnceCell<zbus::Connection> = tokio::sync::OnceCell::const_new();

        // concurrent first calls share one connection attempt
        tokio(async {
            DBUS.get_or_try_init(zbus::Connection::session)
                .await
                .cloned()
        })
        .await
        .wrap_err_with(|| gettext("Failed to connect to the session D-Bus"))
    }

    pub fn window(&self) -> TvWindow {
        let mut window = self.imp().window.borrow_mut();

        match window.as_ref().and_then(|p| p.upgrade()) {
            Some(window) => window,
            None => {
                let new_window = TvWindow::new(self);
                *window = Some(glib::clone::Downgrade::downgrade(&new_window));
                new_window
            }
        }
    }

    pub fn zapp(&self) -> Arc<Zapp> {
        self.imp().zapp.clone()
    }

    pub fn fritzbox(&self) -> Arc<FritzBox> {
        self.imp().fritzbox.clone()
    }

    pub fn live_channels(&self) -> AsyncResource<LiveChannels> {
        self.imp().live_channels.clone()
    }

    pub async fn play(&self, video: VideoInfo) {
        let settings = TvSettings::get();

        if settings.use_external_player() {
            let player_name = settings.video_player_name();
            let player_id = settings.video_player_id();

            let player = if player_id.is_empty() {
                None
            } else {
                match ExternalProgram::find(player_name, player_id.clone()).await {
                    Ok(player) => player,
                    Err(e) => {
                        show_error(e);
                        None
                    }
                }
            };

            let player = match player {
                Some(player) => player,
                None => {
                    match ProgramSelector::select_program(ExternalProgramType::Player, player_id)
                        .await
                    {
                        Some(player) => {
                            settings.set_video_player_name(&player.name);
                            settings.set_video_player_id(&player.id);

                            player
                        }
                        None => return,
                    }
                }
            };

            match player.open(video.uri().to_owned()).await {
                Ok(()) => (),
                Err(e) => show_error(e.wrap_err(gettext("Failed to play video stream"))),
            }
        } else if let Err(e) = mpv::play(video.title(), video.uri(), false).await {
            show_error(e);
        }
    }

    pub async fn download(&self, uri: String) {
        let settings = TvSettings::get();
        let downloader_name = settings.video_downloader_name();
        let downloader_id = settings.video_downloader_id();

        let downloader = if downloader_id.is_empty() {
            None
        } else {
            match ExternalProgram::find(downloader_name, downloader_id.clone()).await {
                Ok(downloader) => downloader,
                Err(e) => {
                    show_error(e);
                    None
                }
            }
        };

        let downloader = match downloader {
            Some(downloader) => downloader,
            None => {
                match ProgramSelector::select_program(
                    ExternalProgramType::Downloader,
                    downloader_id,
                )
                .await
                {
                    Some(downloader) => {
                        settings.set_video_downloader_name(&downloader.name);
                        settings.set_video_downloader_id(&downloader.id);

                        downloader
                    }
                    None => return,
                }
            }
        };

        match downloader.open(uri).await {
            Ok(()) => (),
            Err(e) => show_error(e.wrap_err(gettext("Failed to launch video downloader"))),
        }
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| {
                // quitting ends a running recording, like closing the window
                if TvRecordingState::get().is_recording() {
                    let app = app.clone();
                    spawn(async move {
                        if playback::confirm_quit().await {
                            playback::stop_recording();
                            app.quit();
                        }
                    });
                } else {
                    app.quit();
                }
            })
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| {
                TvPreferencesDialog::new().present(Some(&app.window()))
            })
            .build();
        let download_action = gio::ActionEntry::builder("download")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, variant| {
                if let Some(stream_url) = variant.and_then(|v| v.get()) {
                    spawn_clone!(app => app.download(stream_url))
                }
            })
            .build();

        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            download_action,
        ]);

        self.set_accels_for_action("app.quit", &["<primary>q"]);
        self.set_accels_for_action("app.preferences", &["<primary>comma"]);
        self.set_accels_for_action("window.close", &["<primary>w"]);
        self.set_accels_for_action("window.reload", &["<primary>r", "F5", "Reload"]);
        self.set_accels_for_action("window.search", &["<primary>f"]);
        // like switching tabs, Ctrl+L and Ctrl+T are reserved for the
        // location and new tabs in GNOME
        self.set_accels_for_action("window.show-live", &["<alt>1"]);
        self.set_accels_for_action("window.show-mediathek", &["<alt>2"]);
        self.set_accels_for_action("window.show-fritztv", &["<alt>3"]);
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::from_appdata(
            "/io/github/_45054/TelevidoFritz/io.github._45054.TelevidoFritz.metainfo.xml",
            (PROFILE == "Release").then_some(VERSION),
        );

        about.set_version(VERSION);
        // translators: replace with your names, one per line
        about.set_translator_credits(&gettext("translator-credits"));

        about.present(Some(&self.window()));
    }
}

/// The livestream channels, or why they couldn't be loaded.
pub type LiveChannels = Result<Rc<crate::zapp::ChannelInfoList>, Rc<eyre::Report>>;

#[derive(Debug)]
pub enum VideoInfo {
    Live {
        title: String,
        uri: String,
    },
    Mediathek {
        title: String,
        preferred_quality: VideoQuality,
        uri_high: Option<String>,
        uri_medium: Option<String>,
        uri_low: Option<String>,
    },
}

impl VideoInfo {
    pub fn title(&self) -> &str {
        match self {
            VideoInfo::Live { title, .. } | VideoInfo::Mediathek { title, .. } => title,
        }
    }

    pub fn uri(&self) -> &str {
        match self {
            VideoInfo::Live { uri, .. } => uri,
            VideoInfo::Mediathek {
                preferred_quality,
                uri_high,
                uri_medium,
                uri_low,
                ..
            } => match preferred_quality {
                VideoQuality::High => uri_high.as_deref().expect("no high quality video URI set"),
                VideoQuality::Medium => uri_medium
                    .as_deref()
                    .expect("no medium quality video URI set"),
                VideoQuality::Low => uri_low.as_deref().expect("no low quality video URI set"),
            },
        }
    }
}
