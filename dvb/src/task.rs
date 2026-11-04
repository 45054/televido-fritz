// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Helpers shared by the background tasks of the crate.

use tokio::sync::watch;

/// Resolves when `stop` is set, or when its sender was dropped (e.g. the
/// last handle of the task is gone).
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}
