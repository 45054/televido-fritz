// SPDX-FileCopyrightText: David Cabot <d-k-bo@mailbox.org>
// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{OnceCell, RefCell},
    fmt::Debug,
    future::Future,
    path::Path,
    pin::Pin,
    rc::Rc,
    sync::OnceLock,
    task::{Poll, Waker},
    time::Duration,
};

use adw::{gio, glib, gtk, prelude::*};
use gettextrs::gettext;

use crate::{application::TvApplication, window::TvWindow};

pub use self::async_resource::AsyncResource;

/// The tokio runtime of the application, see [`tokio`].
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
}

pub async fn tokio<Fut, T>(fut: Fut) -> T
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    runtime().spawn(fut).await.expect("tokio thread panicked")
}

pub fn spawn<Fut>(fut: Fut)
where
    Fut: Future<Output = ()> + 'static,
{
    gtk::glib::MainContext::default().spawn_local(fut);
}

macro_rules! spawn_clone {
    ($( $val:ident ),* => $future:expr) => {{
        $(
            let $val = $val.clone();
        )*
        let ctx = glib::MainContext::default();
        ctx.spawn_local(async move { $future.await; });
    }};
}
pub(crate) use spawn_clone;

pub fn format_timestamp_full(t: i64) -> Option<glib::GString> {
    // translators:  %b is Month name (short)
    //				 %-e is the Day number
    //				 %Y is the year (with century)
    //				 %H is the hours (24h format)
    //				 %M is the minutes
    let time_format = gettext("%b %-e, %Y %H:%M");

    glib::DateTime::from_unix_local(t)
        .and_then(|datetime| datetime.format(&time_format))
        .ok()
}

pub fn format_timestamp_time(t: i64) -> Option<glib::GString> {
    // translators:  %H is the hours (24h format)
    //				 %M is the minutes
    let time_format = gettext("%H:%M");

    glib::DateTime::from_unix_local(t)
        .and_then(|datetime| datetime.format(&time_format))
        .ok()
}

pub fn format_duration(duration: &Duration) -> String {
    let mut s = duration.as_secs();

    let h = s / 3_600;
    s %= 3_600;
    let m = s / 60;
    s %= 60;

    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m:02}m{s:02}s")
    }
}

/// Shows the error in a toast. Its “Details” button shows the whole chain of
/// causes.
pub fn show_error(e: eyre::Report) {
    tracing::error!("{e:?}");

    let Some(window) = TvApplication::get()
        .active_window()
        .and_downcast::<TvWindow>()
    else {
        return;
    };

    let message = e.to_string();
    let toast = adw::Toast::builder()
        .title(&message)
        // the message may contain characters like `&` that aren't valid markup
        .use_markup(false)
        .build();

    // the causes, without the backtrace of the debug output
    let causes: Vec<String> = e.chain().skip(1).map(ToString::to_string).collect();
    if !causes.is_empty() {
        toast.set_button_label(Some(&gettext("Details")));
        toast.connect_button_clicked(glib::clone!(
            #[weak]
            window,
            move |_| show_error_details(&window, &message, &causes.join("\n"))
        ));
    }
    window.add_toast(toast);
}

/// Shows `title` in a toast.
pub fn show_toast(title: &str) {
    tracing::info!("{title}");

    if let Some(window) = TvApplication::get()
        .active_window()
        .and_downcast::<TvWindow>()
    {
        window.add_toast(new_toast(title));
    }
}

/// Shows `title` in a toast with a button that opens the folder of the file
/// at `path`.
pub fn show_file_toast(title: &str, path: &Path) {
    tracing::info!("{title}: {}", path.display());

    let Some(window) = TvApplication::get()
        .active_window()
        .and_downcast::<TvWindow>()
    else {
        return;
    };

    let toast = new_toast(title);
    toast.set_button_label(Some(&gettext("Show in Folder")));
    let file = gio::File::for_path(path);
    toast.connect_button_clicked(glib::clone!(
        #[weak]
        window,
        move |_| {
            gtk::FileLauncher::new(Some(&file)).open_containing_folder(
                Some(&window),
                None::<&gio::Cancellable>,
                |result| {
                    if let Err(e) = result {
                        tracing::warn!("failed to open the folder: {e}");
                    }
                },
            );
        }
    ));
    window.add_toast(toast);
}

fn new_toast(title: &str) -> adw::Toast {
    adw::Toast::builder()
        .title(title)
        // the title may contain characters like `&` that aren't valid markup
        .use_markup(false)
        .build()
}

fn show_error_details(window: &TvWindow, message: &str, details: &str) {
    let label = gtk::Label::builder()
        .label(details)
        .selectable(true)
        // a focused label selects all of its text, it is copied with the
        // “Copy” button instead
        .focusable(false)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .css_classes(["monospace"])
        .build();
    let scrolled_window = gtk::ScrolledWindow::builder()
        .child(&label)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();

    let dialog = adw::AlertDialog::builder()
        .heading(message)
        .extra_child(&scrolled_window)
        .build();
    dialog.add_responses(&[("copy", &gettext("_Copy")), ("close", &gettext("_Close"))]);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");

    let text = format!("{message}\n{details}");
    dialog.connect_response(Some("copy"), move |dialog, _| {
        dialog.clipboard().set_text(&text);
    });
    dialog.present(Some(window));
}

pub trait ListStoreExtManual {
    fn typed_insert_sorted<T: IsA<glib::Object>>(
        &self,
        item: &T,
        compare_func: impl FnMut(&T, &T) -> std::cmp::Ordering,
    ) -> u32;
    fn typed_sort<T: IsA<glib::Object>>(
        &self,
        compare_func: impl FnMut(&T, &T) -> std::cmp::Ordering,
    );
}

impl ListStoreExtManual for gio::ListStore {
    fn typed_insert_sorted<T: IsA<glib::Object>>(
        &self,
        item: &T,
        mut compare_func: impl FnMut(&T, &T) -> std::cmp::Ordering,
    ) -> u32 {
        self.insert_sorted(item, |a, b| {
            compare_func(a.downcast_ref().unwrap(), b.downcast_ref().unwrap())
        })
    }
    fn typed_sort<T: IsA<glib::Object>>(
        &self,
        mut compare_func: impl FnMut(&T, &T) -> std::cmp::Ordering,
    ) {
        self.sort(|a, b| compare_func(a.downcast_ref().unwrap(), b.downcast_ref().unwrap()))
    }
}

mod async_resource {
    use super::*;

    #[derive(Clone)]
    pub struct AsyncResource<T: Clone + Debug> {
        inner: Rc<AsyncResourceInner<T>>,
    }

    // not derived, which would require `T: Default`
    impl<T: Clone + Debug> Default for AsyncResource<T> {
        fn default() -> Self {
            Self {
                inner: Rc::new(AsyncResourceInner {
                    data: RefCell::default(),
                    load_fn: OnceCell::default(),
                    wakers: RefCell::default(),
                }),
            }
        }
    }

    impl<T: Clone + Debug> std::fmt::Debug for AsyncResource<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(std::any::type_name::<Self>())
        }
    }

    struct AsyncResourceInner<T: Clone + Debug> {
        data: RefCell<Option<T>>,
        load_fn: OnceCell<LoadFn<T>>,
        wakers: RefCell<Vec<Waker>>,
    }

    type LoadFn<T> = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = T> + 'static>>>;

    impl<T: Clone + Debug> AsyncResource<T> {
        #[track_caller]
        pub fn set_load_fn(
            &self,
            load_fn: impl Fn() -> Pin<Box<dyn Future<Output = T> + 'static>> + 'static,
        ) {
            if self.inner.load_fn.set(Rc::new(load_fn)).is_err() {
                panic!("Resource load function has already been initialized")
            }
        }
    }

    impl<T: 'static + Clone + Debug> AsyncResource<T> {
        pub fn load(&self) {
            match self.inner.load_fn.get().cloned() {
                Some(load_fn) => {
                    // cleared right away, so that awaiting the resource
                    // after `load()` never returns the previous data
                    *self.inner.data.borrow_mut() = None;
                    let inner = self.inner.clone();
                    spawn(async move {
                        let data = load_fn().await;
                        *inner.data.borrow_mut() = Some(data);
                        for waker in inner.wakers.borrow_mut().drain(..) {
                            waker.wake();
                        }
                    });
                }
                None => panic!("Resource load function has not been initialized"),
            }
        }
    }

    impl<T: Clone + Debug> Future for AsyncResource<T> {
        type Output = T;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            match &*self.inner.data.borrow() {
                Some(data) => Poll::Ready(data.clone()),
                None => {
                    self.inner.wakers.borrow_mut().push(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
}
