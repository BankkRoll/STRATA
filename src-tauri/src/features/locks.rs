//! "Why can't I delete this?" (SPEC §15.3, §15.5).
//!
//! `locks_query` names the processes holding a path (Restart Manager) and
//! the running apps whose caches it is. Closing one is a two-step consent
//! round trip: `locks_close_prepare` builds the prompt from a holder **the
//! backend found itself** (the UI only names it by pid and start time, so it
//! cannot dress up a critical process as an ordinary app), and
//! `locks_close_politely` redeems the token. Closing is always polite
//! (`WM_CLOSE` or a Restart Manager shutdown request); nothing is killed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use strata_clean::apps::{RunningAppWarning, running_app_warnings};
use strata_clean::consent::{CloseApp, Prompt};
use strata_clean::locks::{CloseOutcome, DEFAULT_MAX_FILES, LockHolder, close_politely, who_locks};
use tauri::{AppHandle, Manager, Runtime};

use super::consent::{ConsentTicket, PendingConsents};
use super::error::{ErrorKind, FeatureError, FeatureResult, blocking};

/// How long a holder found by a query can be referenced by a close request.
const HOLDER_TTL: Duration = Duration::from_secs(15 * 60);

/// Lock holders the backend has reported recently, keyed by (pid, start
/// time). Only these can be asked to close.
#[derive(Debug, Default)]
pub struct KnownHolders {
    inner: Mutex<HashMap<(u32, u64), (LockHolder, Instant)>>,
}

impl KnownHolders {
    /// Records holders from a query or a pre-flight.
    pub fn remember(&self, holders: impl IntoIterator<Item = LockHolder>) {
        let now = Instant::now();
        let mut m = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.retain(|_, (_, at)| now.saturating_duration_since(*at) < HOLDER_TTL);
        for h in holders {
            m.insert((h.pid, h.start_time), (h, now));
        }
    }

    /// The holder with this identity, if reported recently.
    #[must_use]
    pub fn get(&self, pid: u32, start_time: u64) -> Option<LockHolder> {
        let m = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.get(&(pid, start_time))
            .filter(|(_, at)| at.elapsed() < HOLDER_TTL)
            .map(|(h, _)| h.clone())
    }
}

/// A close request waiting for the user's confirmation.
#[derive(Debug)]
pub struct PendingClose {
    holder: LockHolder,
    prompt: Prompt<CloseApp>,
}

/// Managed state: close prompts awaiting confirmation.
#[derive(Debug, Default)]
pub struct PendingCloses(pub PendingConsents<PendingClose>);

/// Response of `locks_query`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockReport {
    /// Processes holding the path open (`strata_clean::locks::LockHolder`).
    pub holders: Vec<LockHolder>,
    /// "Close X first" warnings for app caches.
    pub running_apps: Vec<RunningAppWarning>,
}

/// Who holds `path` open, and which running apps own it as a cache.
#[tauri::command]
pub async fn locks_query<R: Runtime>(app: AppHandle<R>, path: String) -> FeatureResult<LockReport> {
    blocking(move || {
        let p = PathBuf::from(&path);
        let holders = who_locks(&p, DEFAULT_MAX_FILES)
            .map_err(|e| FeatureError::io("Restart Manager could not check the file", &e))?;
        let running_apps = running_app_warnings(&p, &holders).unwrap_or_default();
        if let Some(k) = app.try_state::<KnownHolders>() {
            k.remember(holders.iter().cloned());
        }
        Ok(LockReport {
            holders,
            running_apps,
        })
    })
    .await
}

/// Builds the "ask X to close" prompt for a holder found by `locks_query`
/// or `cleanup_preflight`. Show `text`, then pass `token` to
/// `locks_close_politely` when the user confirms.
#[tauri::command]
pub fn locks_close_prepare<R: Runtime>(
    app: AppHandle<R>,
    pid: u32,
    start_time: u64,
) -> FeatureResult<ConsentTicket> {
    let holder = app
        .try_state::<KnownHolders>()
        .and_then(|k| k.get(pid, start_time))
        .ok_or_else(|| {
            FeatureError::new(
                ErrorKind::NotFound,
                "that program is not in a recent lock check; check the item again",
            )
        })?;
    let prompt = Prompt::new(holder.close_request());
    let text = prompt.text();
    pending(&app)?.offer(PendingClose { holder, prompt }, text, Instant::now())
}

fn pending<R: Runtime>(app: &AppHandle<R>) -> FeatureResult<&PendingConsents<PendingClose>> {
    app.try_state::<PendingCloses>()
        .map(|s| &s.inner().0)
        .ok_or_else(|| FeatureError::internal("lock state missing"))
}

/// The user confirmed the prompt behind `token`: ask the app to close.
#[tauri::command]
pub async fn locks_close_politely<R: Runtime>(
    app: AppHandle<R>,
    token: String,
) -> FeatureResult<CloseOutcome> {
    let PendingClose { holder, prompt } = pending(&app)?.take(&token, Instant::now())?;
    blocking(move || {
        // This handler is the user's confirmation click; the consent is
        // minted and redeemed on this thread.
        close_politely(&holder, prompt.confirm())
            .map_err(|e| FeatureError::with_detail(ErrorKind::CloseApp, e.to_string(), &e))
    })
    .await
}

/// The user dismissed the prompt.
#[tauri::command]
pub fn locks_close_cancel<R: Runtime>(app: AppHandle<R>, token: String) -> FeatureResult<()> {
    pending(&app)?.cancel(&token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_clean::locks::AppKind;

    fn holder(pid: u32) -> LockHolder {
        LockHolder {
            pid,
            start_time: 9,
            app_name: "Example".into(),
            exe_path: None,
            service: None,
            kind: AppKind::MainWindow,
            restartable: false,
        }
    }

    #[test]
    fn only_reported_holders_can_be_closed() {
        let k = KnownHolders::default();
        assert!(k.get(1, 9).is_none());
        k.remember([holder(1)]);
        assert_eq!(k.get(1, 9).unwrap().app_name, "Example");
        assert!(
            k.get(1, 10).is_none(),
            "a reused pid has another start time"
        );
    }
}
