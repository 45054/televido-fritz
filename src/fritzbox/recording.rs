// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! What is being recorded, for the UI.
//!
//! [`super::playback`] records and updates [`TvRecordingState`]; the cards of
//! the FritzTV page watch it to show their channel's recording, and the
//! window asks before a running recording ends with it.

use std::{
    cell::{Cell, OnceCell, RefCell},
    path::PathBuf,
};

use adw::{glib, prelude::*, subclass::prelude::*};

use crate::settings::TvSettings;

/// The folder in the user's Videos folder that holds the recordings unless
/// the setting `recordings-directory` names another one.
const DEFAULT_SUBDIRECTORY: &str = "Televido";

mod imp {
    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::TvRecordingState)]
    pub struct TvRecordingState {
        /// The stream URL of the channel being recorded, `None` while
        /// nothing records.
        #[property(get)]
        pub(super) url: RefCell<Option<String>>,
        /// The recorded show, or the channel name if the programme guide
        /// doesn't know the show.
        #[property(get)]
        pub(super) title: RefCell<Option<String>>,
        /// When the recording ends, in seconds since the Unix epoch, or 0
        /// if it runs until it is stopped.
        #[property(get)]
        pub(super) until: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TvRecordingState {
        const NAME: &'static str = "TvRecordingState";
        type Type = super::TvRecordingState;
        type ParentType = glib::Object;
    }

    #[glib::derived_properties]
    impl ObjectImpl for TvRecordingState {}
}

glib::wrapper! {
    /// The running recording; one per application, see
    /// [`TvRecordingState::get`].
    pub struct TvRecordingState(ObjectSubclass<imp::TvRecordingState>);
}

impl TvRecordingState {
    /// The state of the application (of the main thread).
    pub fn get() -> Self {
        thread_local! {
            static STATE: OnceCell<TvRecordingState> = const { OnceCell::new() };
        }
        STATE.with(|state| state.get_or_init(glib::Object::new).clone())
    }

    /// Whether a recording is running.
    pub fn is_recording(&self) -> bool {
        self.imp().url.borrow().is_some()
    }

    /// Whether the channel with the stream `url` is being recorded.
    pub fn is_recording_url(&self, url: &str) -> bool {
        self.imp().url.borrow().as_deref() == Some(url)
    }

    /// Sets the state, `url` `None` while nothing records. Only the
    /// changed properties are notified, `url` last, so that its observers
    /// see the matching title and end.
    pub(super) fn set(&self, url: Option<&str>, title: Option<&str>, until: i64) {
        let imp = self.imp();
        if imp.title.borrow().as_deref() != title {
            imp.title.replace(title.map(ToOwned::to_owned));
            self.notify_title();
        }
        if imp.until.replace(until) != until {
            self.notify_until();
        }
        if imp.url.borrow().as_deref() != url {
            imp.url.replace(url.map(ToOwned::to_owned));
            self.notify_url();
        }
    }
}

/// The folder that new recordings are written to: the setting
/// `recordings-directory`, or by default `Televido` in the user's Videos
/// folder.
pub fn recordings_directory() -> PathBuf {
    let custom = TvSettings::get().recordings_directory();
    if !custom.trim().is_empty() {
        return PathBuf::from(custom);
    }
    // without a configured Videos folder, e.g. if xdg-user-dirs is missing
    glib::user_special_dir(glib::UserDirectory::Videos)
        .unwrap_or_else(glib::home_dir)
        .join(DEFAULT_SUBDIRECTORY)
}
