//! Cross-platform process, service and console plumbing.
//!
//! Everything WARP-related in ocinfinity has to work on Windows as well as on
//! systemd-based Linux. Historically every one of these call sites shelled out
//! to `sudo`/`systemctl`/`sh -c`, which silently no-ops on Windows and makes
//! `ocinfinity reset` do nothing at all there. All of it is funnelled through this
//! module instead.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, warn};

/// Windows service name registered by the Cloudflare WARP client.
#[cfg(windows)]
const WARP_SERVICE: &str = "CloudflareWARP";

/// systemd unit used by the Linux WARP client.
#[cfg(not(windows))]
const WARP_SERVICE: &str = "warp-svc";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Disable,
}

/// Resolve an executable against `PATH`, honouring `PATHEXT` on Windows.
///
/// `Command::new` already searches `PATH`, but the Windows WARP installer puts
/// `warp-cli.exe` in `C:\Program Files (x86)\Cloudflare\Cloudflare WARP\`
/// *without* adding it to `PATH`, so we need an explicit resolver.
pub fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;

    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &extensions {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                let wanted = ext.to_ascii_lowercase();
                if !name.to_ascii_lowercase().ends_with(&wanted) {
                    candidate.set_extension(ext.trim_start_matches('.'));
                }
            }
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Extra install locations probed when `warp-cli` is not on `PATH`.
fn warp_cli_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if cfg!(windows) {
        for base in [
            r"C:\Program Files (x86)\Cloudflare\Cloudflare WARP",
            r"C:\Program Files\Cloudflare\Cloudflare WARP",
        ] {
            out.push(PathBuf::from(base).join("warp-cli.exe"));
        }
    }

    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/opt/homebrew/bin/warp-cli"));
        out.push(PathBuf::from("/usr/local/bin/warp-cli"));
    }

    if cfg!(any(target_os = "linux", target_os = "macos")) {
        out.push(PathBuf::from("/usr/bin/warp-cli"));
        out.push(PathBuf::from("/usr/local/bin/warp-cli"));
    }

    out
}

/// Absolute path to `warp-cli`, or the bare command name if it was not found.
///
/// Returning the bare name rather than `None` keeps the error messages honest:
/// the caller's own "command not found" is more precise than a false negative.
pub fn warp_cli_path() -> OsString {
    if let Some(found) = which("warp-cli") {
        return found.into_os_string();
    }
    for candidate in warp_cli_candidates() {
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    OsString::from("warp-cli")
}

/// Run a program and return its stdout, or `None` when it cannot be launched.
///
/// Used for short informational queries (`opencode service status`) where a
/// missing binary or a non-zero exit is a fact to report, not an error.
pub fn command_output(program: &std::path::Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program).args(args).output().ok()?;
    if output.status.success() {
        Some(output.stdout)
    } else {
        None
    }
}

pub fn is_warp_installed() -> bool {
    let path = warp_cli_path();
    match Command::new(&path).arg("--version").output() {
        Ok(out) => out.status.success() || !out.stdout.is_empty(),
        Err(_) => false,
    }
}

/// Directory holding the WARP client's local state.
///
/// Only meaningful on Linux: on Windows the service owns this directory and the
/// supported way to force re-registration is `warp-cli registration delete`.
pub fn warp_state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        None
    } else {
        Some(PathBuf::from("/var/lib/cloudflare-warp"))
    }
}

/// Run `warp-cli` and return its trimmed stdout.
pub async fn run_warp(args: &[&str]) -> Result<String, String> {
    let path = warp_cli_path();
    let output = tokio::process::Command::new(&path)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to execute warp-cli: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!(
            "warp-cli {} failed: {}",
            args.join(" "),
            if detail.is_empty() {
                "unknown error"
            } else {
                &detail
            }
        ))
    }
}

/// Synchronous variant of [`run_warp`] for the CLI paths that are not async.
pub fn run_warp_blocking(args: &[&str]) -> Result<String, String> {
    let path = warp_cli_path();
    let output = Command::new(&path)
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute warp-cli: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("warp-cli {} failed: {}", args.join(" "), stderr))
    }
}

/// Force a fresh WARP registration.
///
/// Both verbs are mandatory and must run in this order. `registration delete`
/// drops the identity but does **not** create a new one: the client is left in
/// `Unable / Registration Missing` and `connect` still exits 0 while doing
/// nothing, so a rotation that stops after `delete` silently destroys the
/// tunnel. This was exactly the failure mode that stranded WARP on
/// `2026.8.2115.0`. `registration new` must always follow, even when `delete`
/// reports that there was nothing to delete.
pub async fn reregister() -> Result<String, String> {
    if let Err(e) = run_warp(&["registration", "delete"]).await {
        debug!("registration delete reported: {}", e);
    }
    run_warp(&["registration", "new"]).await
}

pub fn reregister_blocking() -> Result<String, String> {
    if let Err(e) = run_warp_blocking(&["registration", "delete"]) {
        debug!("registration delete reported: {}", e);
    }
    run_warp_blocking(&["registration", "new"])
}

/// Observed state of the WARP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpStatus {
    Connected,
    Disconnected,
    /// The client has no usable registration and will not connect on its own.
    Unable,
    Unknown,
}

/// Classify `warp-cli status` output.
///
/// Order matters: `"disconnected"` contains `"connected"`, and an `Unable`
/// line may also mention a healthy daemon, so the failure states are tested
/// first.
pub fn parse_warp_status(text: &str) -> WarpStatus {
    let lower = text.to_lowercase();
    if lower.contains("unable") || lower.contains("missing") {
        WarpStatus::Unable
    } else if lower.contains("disconnected") {
        WarpStatus::Disconnected
    } else if lower.contains("connected") {
        WarpStatus::Connected
    } else {
        WarpStatus::Unknown
    }
}

pub fn warp_status_blocking() -> WarpStatus {
    match run_warp_blocking(&["status"]) {
        Ok(text) => parse_warp_status(&text),
        Err(_) => WarpStatus::Unknown,
    }
}

pub async fn warp_status() -> WarpStatus {
    match run_warp(&["status"]).await {
        Ok(text) => parse_warp_status(&text),
        Err(_) => WarpStatus::Unknown,
    }
}

/// Block until the client reports a connected tunnel, or the deadline expires.
pub fn wait_for_connection_blocking(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if warp_status_blocking() == WarpStatus::Connected {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub async fn wait_for_connection(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if warp_status().await == WarpStatus::Connected {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Re-establish a working tunnel after the identity was rotated.
///
/// Registering and connecting is retried once, because a fresh registration can
/// race the daemon coming back from a service cycle. The caller gets an error
/// only when the tunnel is genuinely not up, never a silent success.
pub async fn restore_tunnel() -> Result<(), String> {
    for attempt in 1..=2 {
        if let Err(e) = reregister().await {
            debug!("registration attempt {} reported: {}", attempt, e);
        }
        if let Err(e) = run_warp(&["connect"]).await {
            debug!("connect attempt {} reported: {}", attempt, e);
        }
        if wait_for_connection(Duration::from_secs(15)).await {
            return Ok(());
        }
        warn!("tunnel not connected after attempt {}", attempt);
    }
    Err("WARP did not reconnect after re-registering; run `warp-cli registration new` and `warp-cli connect`".to_string())
}

pub fn restore_tunnel_blocking() -> Result<(), String> {
    for attempt in 1..=2 {
        if let Err(e) = reregister_blocking() {
            debug!("registration attempt {} reported: {}", attempt, e);
        }
        if let Err(e) = run_warp_blocking(&["connect"]) {
            debug!("connect attempt {} reported: {}", attempt, e);
        }
        if wait_for_connection_blocking(Duration::from_secs(15)) {
            return Ok(());
        }
        warn!("tunnel not connected after attempt {}", attempt);
    }
    Err("WARP did not reconnect after re-registering; run `warp-cli registration new` and `warp-cli connect`".to_string())
}

/// Start/stop/disable the WARP daemon service, elevating when required.
pub fn control_service(action: ServiceAction) -> Result<String, String> {
    let verb = match action {
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
        ServiceAction::Disable => "disable",
    };

    if cfg!(windows) {
        return elevate("sc.exe", &[verb, WARP_SERVICE]);
    }

    if which("systemctl").is_none() {
        return Err("systemctl not found; control the WARP service manually".to_string());
    }

    // macOS runs WARP as a per-user launchd agent that the invoking user can
    // drive directly, so elevation is neither needed nor desirable there.
    if cfg!(target_os = "macos") {
        run_plain("systemctl", &[verb, WARP_SERVICE])
    } else {
        elevate("systemctl", &[verb, WARP_SERVICE])
    }
}

/// Wipe the WARP client's cached registration data (Linux only).
pub fn clear_warp_state() -> Result<String, String> {
    match warp_state_dir() {
        Some(dir) => {
            let pattern = format!("{}/*", dir.display());
            elevate("rm", &["-rf", &pattern])
        }
        // Windows keeps its state under the service's own directory and will
        // refuse to let us delete it; `reregister()` covers that platform.
        None => Ok("skipped: Windows WARP state is service-owned".to_string()),
    }
}

fn run_plain(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute {}: {}", program, e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Run `program args` with elevated privileges.
///
/// Linux/macOS use `sudo -n` (never blocks on a password prompt, so an
/// automated reset cannot hang forever). Windows re-launches through PowerShell
/// `-Verb RunAs`, which raises a UAC prompt exactly once.
pub fn elevate(program: &str, args: &[&str]) -> Result<String, String> {
    #[cfg(windows)]
    {
        let _ = (program, args);
        return elevate_windows(program, args);
    }

    #[cfg(not(windows))]
    {
        elevate_unix(program, args)
    }
}

#[cfg(not(windows))]
fn elevate_unix(program: &str, args: &[&str]) -> Result<String, String> {
    if which("sudo").is_none() {
        return Err(format!(
            "sudo is required to run `{} {}`",
            program,
            args.join(" ")
        ));
    }

    // `sudo -n` fails fast instead of prompting, which is what we want inside a
    // rate-limit recovery path: a hanging reset is worse than a failed one.
    let output = Command::new("sudo")
        .arg("-n")
        .arg("--")
        .arg(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute sudo: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!(
            "elevated `{} {}` failed: {} (run `sudo -v` first to cache credentials)",
            program,
            args.join(" "),
            detail
        ))
    }
}

#[cfg(windows)]
fn elevate_windows(program: &str, args: &[&str]) -> Result<String, String> {
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");

    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $p = Start-Process -FilePath '{prog}' -ArgumentList @({args}) \
         -Verb RunAs -Wait -PassThru -WindowStyle Hidden; \
         exit $p.ExitCode",
        prog = program.replace('\'', "''"),
        args = arg_list,
    );

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .map_err(|e| format!("failed to execute powershell: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "elevated `{} {}` failed: {}",
            program,
            args.join(" "),
            if stderr.is_empty() {
                "cancelled or denied"
            } else {
                stderr.as_str()
            }
        ))
    }
}

/// Force UTF-8 on the Windows console so the banner and box-drawing glyphs do
/// not come out as mojibake under `conhost` / `cmd.exe`.
pub fn enable_utf8_console() {
    #[cfg(windows)]
    unsafe {
        // 65001 == CP_UTF8. Both the input and output code pages are set so
        // that interactive prompts (model selection) round-trip correctly too.
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
        windows_sys::Win32::System::Console::SetConsoleCP(65001);
    }
}

/// Human-readable description of the active platform, for `ocinfinity about`.
pub fn os_label() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "unknown"
    }
}

/// Whether the full tunnel reset (service restart + cache wipe) is supported.
pub fn supports_full_reset() -> bool {
    cfg!(any(target_os = "windows", target_os = "linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_the_rust_host_triple_binary() {
        // `rustc` is on PATH in any environment that can run these tests.
        let found = which("rustc").expect("rustc should be resolvable");
        assert!(found.is_file(), "{:?} is not a file", found);
    }

    #[test]
    fn which_returns_none_for_a_missing_binary() {
        assert!(which("definitely-not-a-real-binary-8f3a").is_none());
    }

    #[test]
    fn warp_cli_path_always_yields_something_callable() {
        // Either a resolved absolute path or the bare command name; never an
        // empty string, which would make Command::new panic-free but useless.
        assert!(!warp_cli_path().is_empty());
    }

    #[test]
    fn warp_status_parses_every_documented_state() {
        assert_eq!(
            parse_warp_status("Status update: Connected\nNetwork: healthy"),
            WarpStatus::Connected
        );
        assert_eq!(
            parse_warp_status("Status update: Disconnected\nReason: Unspecified"),
            WarpStatus::Disconnected
        );
        assert_eq!(
            parse_warp_status(
                "Status update: Unable\nReason: Registration Missing due to: Manual deletion"
            ),
            WarpStatus::Unable
        );
        assert_eq!(parse_warp_status(""), WarpStatus::Unknown);
    }

    #[test]
    fn warp_status_never_reads_disconnected_as_connected() {
        // "disconnected" contains "connected"; the ordering in the parser is
        // what keeps a dead tunnel from being reported as healthy.
        assert_ne!(
            parse_warp_status("Status update: Disconnected"),
            WarpStatus::Connected
        );
    }

    #[test]
    fn service_name_is_platform_correct() {
        if cfg!(windows) {
            assert_eq!(WARP_SERVICE, "CloudflareWARP");
        } else {
            assert_eq!(WARP_SERVICE, "warp-svc");
        }
    }
}
