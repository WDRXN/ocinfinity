use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::platform::{self, ServiceAction};

/// Number of resets currently in flight. Used only for log context, but it
/// must never drift: it used to be decremented on the success path alone, so a
/// single failed reset permanently inflated the counter.
static ACTIVE_RESETS: AtomicU32 = AtomicU32::new(0);

/// Serialises resets so parallel requests through the proxy rotate the
/// identity once instead of N times.
static RESET_LOCK: Mutex<()> = Mutex::const_new(());

pub struct WarpResolver {
    pub max_retries: u32,
    pub reset_delay_ms: u64,
}

impl WarpResolver {
    pub fn new(max_retries: u32, reset_delay_ms: u64) -> Self {
        Self {
            max_retries,
            reset_delay_ms,
        }
    }

    /// React to an upstream 429 by rotating the WARP egress identity.
    ///
    /// Returns `true` when the caller may retry the upstream request. A reset
    /// already underway by another request counts as success: the other task is
    /// doing exactly what we wanted, and queuing a second reset behind it would
    /// only churn the tunnel.
    pub async fn handle_429(&self, retry_count: u32) -> bool {
        if retry_count >= self.max_retries {
            warn!(
                "max retry attempts ({}) reached, giving up",
                self.max_retries
            );
            return false;
        }

        // Never escalate without WARP present. Every reset step needs service
        // control, which on Windows means a UAC prompt; firing that in the
        // middle of an HTTP request would stall the caller for an unbounded
        // time and still cannot succeed.
        if !platform::is_warp_installed() {
            warn!("WARP is not installed; cannot rotate the egress identity");
            return false;
        }

        let _guard = match RESET_LOCK.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                info!("WARP reset already in progress; waiting for it to settle");
                tokio::time::sleep(Duration::from_millis(self.reset_delay_ms)).await;
                return true;
            }
        };

        ACTIVE_RESETS.fetch_add(1, Ordering::SeqCst);
        let in_flight = ACTIVE_RESETS.load(Ordering::SeqCst);
        info!(
            "HTTP 429 detected; rotating WARP identity (in flight: {})",
            in_flight
        );

        let result = self.reset_warp().await;

        // Decrement on every exit path, success or failure.
        ACTIVE_RESETS.fetch_sub(1, Ordering::SeqCst);

        if let Err(e) = result {
            error!("WARP reset failed: {}", e);
            return false;
        }

        info!(
            "WARP reset complete; waiting {}ms for the tunnel to stabilise",
            self.reset_delay_ms
        );
        tokio::time::sleep(Duration::from_millis(self.reset_delay_ms)).await;
        true
    }

    /// The full reset sequence: disconnect, cycle the service, drop the cached
    /// registration, re-register, reconnect.
    ///
    /// Every step is best-effort except the ones without which the tunnel never
    /// comes back, so a systemd-less host still gets a working rotation.
    pub async fn reset_warp(&self) -> Result<(), String> {
        if !platform::is_warp_installed() {
            return Err("WARP is not installed; run `ocinfinity install warp` first".to_string());
        }

        info!("step 1/6: warp-cli disconnect");
        match platform::run_warp(&["disconnect"]).await {
            Ok(_) => {}
            // Not connected is the normal case when a rate limit arrives while
            // the tunnel is already down.
            Err(e) if e.contains("not connected") || e.contains("Disconnected") => {
                info!("tunnel was already disconnected");
            }
            Err(e) => warn!("disconnect reported: {}", e),
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;

        info!("step 2/6: stop WARP service");
        let stopped = match platform::control_service(ServiceAction::Stop) {
            Ok(_) => true,
            Err(e) => {
                warn!("stopping the WARP service failed or was skipped: {}", e);
                false
            }
        };

        tokio::time::sleep(Duration::from_millis(500)).await;

        info!("step 3/6: clear cached WARP registration");
        match platform::clear_warp_state() {
            Ok(_) if stopped => {}
            Ok(note) => info!("skipped: {}", note),
            Err(e) => warn!("clearing the WARP cache failed or was skipped: {}", e),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        info!("step 4/6: start WARP service");
        if let Err(e) = platform::control_service(ServiceAction::Start) {
            warn!("starting the WARP service failed or was skipped: {}", e);
        }

        tokio::time::sleep(Duration::from_millis(2000)).await;

        info!("step 5/6: register a fresh WARP identity and reconnect");
        platform::restore_tunnel().await?;

        info!("step 6/6: verify the tunnel");
        if platform::warp_status().await != platform::WarpStatus::Connected {
            return Err("WARP reports it is not connected after the reset".to_string());
        }

        info!("WARP tunnel reset complete");
        Ok(())
    }

    /// The lighter rotation used by `ocinfinity quick-reset`: no service restart,
    /// no cache wipe, just a new registration.
    pub async fn quick_reset(&self) -> Result<(), String> {
        info!("quick reset: disconnect");
        let _ = platform::run_warp(&["disconnect"]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        info!("quick reset: new registration");
        platform::restore_tunnel().await?;

        if platform::warp_status().await != platform::WarpStatus::Connected {
            return Err("WARP is not connected after the quick reset".to_string());
        }

        info!("quick reset complete");
        Ok(())
    }

    /// Blocking variant of [`Self::quick_reset`] for the synchronous CLI paths.
    pub fn quick_reset_blocking(&self) -> Result<(), String> {
        let _ = platform::run_warp_blocking(&["disconnect"]);
        platform::restore_tunnel_blocking()?;

        if platform::warp_status_blocking() != platform::WarpStatus::Connected {
            return Err("WARP is not connected after the quick reset".to_string());
        }
        Ok(())
    }
}
