//! Parse-phase progress reporting for usage scans.
//!
//! Progress is published while files are being (re)parsed — the phase that can
//! take minutes on a cold cache — and consumed by CLI/TUI progress surfaces.
//! Cache hits are not counted.

use anyhow::Result;
use once_cell::sync::Lazy;
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;

/// Parse-phase progress of the usage scan currently refreshing. Cache hits
/// are not counted: progress is only published while files are being (re)parsed,
/// which is the phase that can take minutes on a cold cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UsageScanProgress {
    pub source: &'static str,
    pub done: usize,
    pub total: usize,
}

static USAGE_SCAN_PROGRESS: Lazy<Mutex<Option<UsageScanProgress>>> = Lazy::new(|| Mutex::new(None));

pub fn usage_scan_progress() -> Option<UsageScanProgress> {
    *USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forward advancing cold-cache counters without imposing a total scan lifetime.
/// Callers retain their own cancellation and inactivity watchdogs.
pub(crate) fn with_usage_progress<T: Send>(
    action: impl FnOnce() -> T + Send,
    mut publish: impl FnMut(UsageScanProgress) -> Result<()>,
) -> Result<T> {
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = sender.send(action());
        });
        let mut previous = None;
        loop {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(result) => return Ok(result),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("activity worker stopped without a result"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(progress) = usage_scan_progress()
                        && Some(progress) != previous
                    {
                        publish(progress)?;
                        previous = Some(progress);
                    }
                }
            }
        }
    })
}

pub(crate) fn publish_scan_progress(progress: Option<UsageScanProgress>) {
    *USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = progress;
}

pub(crate) fn publish_remote_scan_progress(progress: UsageScanProgress) {
    publish_scan_progress(Some(progress));
}

pub(crate) fn bump_scan_progress() {
    if let Some(progress) = USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        progress.done += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // TODO(split): point at snapshot::USAGE_SCAN_LOCK once the lock moves there.
    use super::super::snapshot::USAGE_SCAN_LOCK;

    #[test]
    fn activity_reports_progress_before_the_scan_completes() {
        let _scan_guard = USAGE_SCAN_LOCK.lock().unwrap();
        let (acknowledge, received) = std::sync::mpsc::channel();
        let mut updates = Vec::new();
        let value = with_usage_progress(
            move || {
                for done in [0, 1] {
                    publish_scan_progress(Some(UsageScanProgress {
                        source: "codex",
                        done,
                        total: 2,
                    }));
                    received
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap();
                }
                publish_scan_progress(None);
                42
            },
            |progress| {
                updates.push(progress.done);
                acknowledge.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(updates, vec![0, 1]);
    }
}
