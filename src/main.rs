use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use colored::Colorize;
use ocinfinity::detect;
use ocinfinity::platform::{self, ServiceAction};
use ocinfinity::proxy::server::resolve_listen_addr;
use ocinfinity::{AppConfig, ProxyConfig};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "ocinfinity")]
#[command(version = VERSION)]
#[command(about = "OpenCode limit reset + Claude Code proxy bridge", long_about = None)]
struct Cli {
    /// Enable debug logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Print the commands that would run, without running them
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show WARP tunnel status
    Status {
        /// Emit machine-readable JSON only
        #[arg(long)]
        json: bool,
    },
    /// Full WARP tunnel reset (service restart + new registration)
    Reset {},
    /// Fast WARP IP rotation (no service restart)
    QuickReset {},
    /// Install a dependency
    Install {
        #[command(subcommand)]
        target: InstallTarget,
    },
    /// Stop the WARP tunnel and disable its service
    Stop {},
    /// Show version and command reference
    About {},
    /// Start the Anthropic <-> OpenCode Zen proxy
    Proxy {
        /// Address the proxy listens on (auto-picks a free port when omitted)
        #[arg(long, env = "OCINFINITY_LISTEN")]
        listen: Option<String>,
        /// OpenCode Zen base URL (auto-detected when omitted)
        #[arg(long, env = "OCINFINITY_UPSTREAM")]
        upstream: Option<String>,
        /// API key for OpenCode Zen, if it requires one
        #[arg(long, env = "OCINFINITY_API_KEY")]
        api_key: Option<String>,
        /// WARP reset attempts before giving up
        #[arg(long, default_value = "3")]
        max_retries: u32,
        /// Settle time after a WARP reset, in milliseconds
        #[arg(long, default_value = "5000")]
        warp_delay: u64,
    },
    /// Start the proxy, then launch Claude Code wired up to it
    Connect {
        #[command(subcommand)]
        target: ConnectTarget,
    },
    /// Poll OpenCode Zen and rotate WARP whenever it reports 429
    Watch {
        /// OpenCode Zen base URL (auto-detected when omitted)
        #[arg(long, env = "OCINFINITY_UPSTREAM")]
        upstream: Option<String>,
        #[arg(long, env = "OCINFINITY_API_KEY")]
        api_key: Option<String>,
        #[arg(long, default_value = "3")]
        max_retries: u32,
        #[arg(long, default_value = "5000")]
        warp_delay: u64,
    },
    /// Run the proxy in the foreground until interrupted
    Daemon {
        /// Address the proxy listens on (auto-picks a free port when omitted)
        #[arg(long, env = "OCINFINITY_LISTEN")]
        listen: Option<String>,
        /// OpenCode Zen base URL (auto-detected when omitted)
        #[arg(long, env = "OCINFINITY_UPSTREAM")]
        upstream: Option<String>,
        #[arg(long, env = "OCINFINITY_API_KEY")]
        api_key: Option<String>,
        #[arg(long, default_value = "3")]
        max_retries: u32,
        #[arg(long, default_value = "5000")]
        warp_delay: u64,
    },
    /// Manage the saved configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Diagnose the local setup
    Doctor {},
    /// Detect OpenCode, Claude Code, WARP and any running proxy
    Detect {
        /// Emit machine-readable JSON only
        #[arg(long)]
        json: bool,
    },
    /// Interactive first-run installer
    Setup {},
    /// List the models OpenCode Zen exposes
    Models {
        /// OpenCode Zen base URL (auto-detected when omitted)
        #[arg(long, env = "OCINFINITY_UPSTREAM")]
        upstream: Option<String>,
        #[arg(long, env = "OCINFINITY_API_KEY")]
        api_key: Option<String>,
    },
    /// Print a shell completion script
    Completions {
        #[arg(value_name = "SHELL")]
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
enum InstallTarget {
    /// Install Cloudflare WARP
    Warp {},
    /// Install OpenCode
    Opencode {},
    /// Install Claude Code CLI
    ClaudeCode {},
    /// Install WARP + OpenCode + Claude Code
    All {},
}

#[derive(Subcommand, Debug)]
enum ConnectTarget {
    /// Start the proxy and launch Claude Code against it
    ClaudeCode {
        /// Address the proxy listens on (auto-picks a free port when omitted)
        #[arg(long, env = "OCINFINITY_LISTEN")]
        listen: Option<String>,
        /// OpenCode Zen base URL (auto-detected when omitted)
        #[arg(long, env = "OCINFINITY_UPSTREAM")]
        upstream: Option<String>,
        #[arg(long, env = "OCINFINITY_API_KEY")]
        api_key: Option<String>,
        #[arg(long, default_value = "3")]
        max_retries: u32,
        #[arg(long, default_value = "5000")]
        warp_delay: u64,
        /// Select this model instead of prompting
        #[arg(long)]
        model: Option<String>,
        /// Extra system prompt appended for the session
        #[arg(long)]
        system_prompt: Option<String>,
        /// Extra arguments forwarded to `claude` (place them after `--`)
        #[arg(last = true)]
        claude_args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Print the effective configuration
    Show {},
    /// Update individual keys (unspecified keys keep their current value)
    Set {
        /// Listen address, e.g. `127.0.0.1:9000`
        #[arg(long)]
        listen: Option<String>,
        /// OpenCode Zen base URL
        #[arg(long)]
        upstream: Option<String>,
        /// API key for OpenCode Zen
        #[arg(long)]
        api_key: Option<String>,
        /// WARP reset attempts
        #[arg(long)]
        max_retries: Option<u32>,
        /// Settle time after a WARP reset, in milliseconds
        #[arg(long)]
        warp_delay: Option<u64>,
        /// Clear the stored API key
        #[arg(long)]
        clear_api_key: bool,
    },
    /// Delete the saved configuration
    Reset {},
}

// ---------------------------------------------------------------------------
// Paths and configuration
// ---------------------------------------------------------------------------

/// Per-user configuration file.
///
/// `dirs::config_dir()` covers the platform conventions properly. The previous
/// hand-rolled resolver read `HOME`/`XDG_CONFIG_HOME`, neither of which is set
/// on Windows, so every `config` command silently wrote `./ocinfinity/config.json`
/// into whatever directory the shell happened to be in.
fn config_path() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("ocinfinity");
    path.push("config.json");
    path
}

fn load_config() -> AppConfig {
    let path = config_path();
    if !path.exists() {
        return AppConfig::default();
    }
    match fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<AppConfig>(&content) {
            Ok(config) => config,
            Err(e) => {
                eprintln!(
                    "{} {}: {}; using defaults",
                    "[WARN]".yellow(),
                    path.display(),
                    e
                );
                AppConfig::default()
            }
        },
        Err(e) => {
            eprintln!(
                "{} {}: {}; using defaults",
                "[WARN]".yellow(),
                path.display(),
                e
            );
            AppConfig::default()
        }
    }
}

fn save_config(config: &AppConfig) -> Result<PathBuf, String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// External tool resolution
// ---------------------------------------------------------------------------

/// Locate an external tool, resolving the `.cmd`/`.bat` shims that npm installs
/// on Windows.
///
/// `CreateProcess` cannot execute those directly, so the returned flag tells
/// the caller to route through `cmd.exe /c`. Without this, `npm install -g
/// opencode-ai` and every other npm-based installer failed on Windows with a
/// misleading "program not found" error even though `npm` was on `PATH`.
fn resolve_tool(name: &str) -> Option<(PathBuf, bool)> {
    let path = platform::which(name)?;
    let is_script = path
        .extension()
        .map(|ext| {
            let ext = ext.to_string_lossy().to_ascii_lowercase();
            ext == "cmd" || ext == "bat"
        })
        .unwrap_or(false);
    Some((path, is_script))
}

/// Build a `Command` for `name args`, handling Windows script shims.
fn tool_command(name: &str, args: &[&str]) -> Command {
    match resolve_tool(name) {
        Some((path, true)) => {
            let mut cmd = Command::new("cmd");
            cmd.arg("/c").arg(&path).args(args);
            cmd
        }
        Some((path, false)) => {
            let mut cmd = Command::new(&path);
            cmd.args(args);
            cmd
        }
        None => {
            let mut cmd = Command::new(name);
            cmd.args(args);
            cmd
        }
    }
}

fn tool_exists(name: &str) -> bool {
    resolve_tool(name).is_some()
}

/// Whether an npm package is currently installed globally.
///
/// `npm ls -g` exits non-zero for a missing package, which [`run_command`]
/// surfaces as `Err`, so any `Ok` carrying the package name means "present".
fn npm_package_installed(package: &str) -> bool {
    run_command("npm", &["ls", "-g", "--depth=0", package])
        .map(|out| out.contains(package))
        .unwrap_or(false)
}

fn run_command(name: &str, args: &[&str]) -> Result<String, String> {
    let output = tool_command(name, args)
        .output()
        .map_err(|e| format!("failed to run `{}`: {}", name, e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn run_interactive(name: &str, args: &[&str]) -> Result<(), String> {
    let status = tool_command(name, args)
        .status()
        .map_err(|e| format!("failed to launch `{}`: {}", name, e))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` exited with {}", name, status))
    }
}

/// Re-run this binary with `args`.
///
/// `setup` and `install all` delegate to the `install` subcommands. Resolving
/// `ocinfinity` through `PATH` failed whenever the tool was run from a build
/// directory or a release archive, so the current executable is used instead.
fn run_self(args: &[&str]) -> Result<(), String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ocinfinity"));
    let status = Command::new(&exe)
        .args(args)
        .status()
        .map_err(|e| format!("failed to re-run {}: {}", exe.display(), e))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {}", exe.display(), status))
    }
}

/// Execute a bare program with elevated privileges.
///
/// Used by installers whose steps are single commands rather than shell
/// pipelines (e.g. the Windows WARP installer steps: `curl.exe ...`, `msiexec
/// /i ...`). Shell-style scripts (Unix installers using `apt-get`/`dnf`) go
/// through [`run_elevated_shell`] instead.
fn run_elevated_cmd(
    program: &str,
    args: &[&str],
    dry_run: bool,
    verbose: bool,
) -> Result<String, String> {
    if dry_run {
        let message = format!(
            "[DRY-RUN] would run (elevated): {} {}",
            program,
            args.join(" ")
        );
        eprintln!("{}", message.cyan());
        return Ok(message);
    }
    if verbose {
        eprintln!(
            "{} running: {} {}",
            "[DEBUG]".cyan(),
            program,
            args.join(" ")
        );
    }
    platform::elevate(program, args)
}

/// Execute a shell pipeline with elevated privileges.
///
/// Used by the WARP installer, whose Unix steps are shell pipelines (`curl |
/// gpg`) that cannot be expressed as a bare program plus arguments.
fn run_elevated_shell(script: &str, dry_run: bool, verbose: bool) -> Result<String, String> {
    if dry_run {
        let message = format!("[DRY-RUN] would run (elevated): {}", script);
        eprintln!("{}", message.cyan());
        return Ok(message);
    }
    if verbose {
        eprintln!("{} running: {}", "[DEBUG]".cyan(), script);
    }
    platform::elevate("sh", &["-c", script])
}

fn probe_port(addr: &str) -> bool {
    match resolve_listen_addr(addr) {
        Ok(socket) => std::net::TcpListener::bind(socket).is_ok(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn print_banner() {
    // ASCII only: box-drawing glyphs depend on the console code page and show
    // up as mojibake on legacy Windows terminals.
    let title = "O C I N F I N I T Y";
    let subtitle = "OpenCode limit reset / Claude Code bridge";
    let inner = title.chars().count().max(subtitle.chars().count()) + 6;
    let pad = |text: &str| {
        let used = text.chars().count();
        format!("{}{}", text, " ".repeat(inner.saturating_sub(used + 2)))
    };

    println!(
        "{}",
        format!(
            "\n  +{}+\n  | {} |\n  | {} |\n  +{}+",
            "-".repeat(inner),
            pad(title),
            pad(subtitle),
            "-".repeat(inner)
        )
        .bold()
        .cyan()
    );
}

fn print_step(num: usize, total: usize, text: &str) {
    println!(
        "  {} {}",
        format!("[{}/{}]", num, total).dimmed(),
        text.bold()
    );
}

fn print_success(text: &str) {
    println!("  {} {}", "✓".green().bold(), text);
}

fn print_fail(text: &str) {
    println!("  {} {}", "✗".red().bold(), text);
}

fn print_info_formatted(label: &str, desc: &str) {
    println!("  {} {} — {}", "→".cyan(), label.bold(), desc.dimmed());
}

/// Install a tracing subscriber writing to stderr, so log lines never
/// interleave with anything a caller pipes from stdout.
///
/// The proxy previously called `tracing::info!` from every code path while no
/// subscriber existed anywhere, which is why it ran completely silent.
fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let default = if verbose {
        "ocinfinity=debug,info"
    } else {
        "ocinfinity=info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

fn fetch_models(upstream: &str, api_key: &Option<String>) -> Result<Vec<(String, String)>, String> {
    let url = detect::endpoint(upstream, "/v1/models");

    // The OpenCode gateway always requires a key; `public` is the value its own
    // client sends. Passing it to a local server that ignores auth is harmless.
    let key = api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| Some(detect::DEFAULT_OPENCODE_API_KEY.to_string()));

    let mut request = reqwest::blocking::Client::builder()
        .user_agent(detect::USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .timeout(Duration::from_secs(10));

    if let Some(key) = key {
        request = request.header("Authorization", format!("Bearer {}", key));
    }

    let resp = request.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let data = resp
        .json::<serde_json::Value>()
        .map_err(|e| e.to_string())?;
    let models = data
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "no `data` array in the upstream response".to_string())?;

    Ok(models
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            if id.is_empty() {
                return None;
            }
            let name = m
                .get("name")
                .or_else(|| m.get("display_name"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| title_case(&id));
            Some((id, name))
        })
        .collect())
}

fn title_case(id: &str) -> String {
    id.replace('-', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => format!(
                    "{}{}",
                    first.to_uppercase().collect::<String>(),
                    chars.as_str()
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn select_model_interactively(models: &[(String, String)]) -> Option<String> {
    use std::io::{self, Write};

    if models.is_empty() {
        return None;
    }

    println!();
    println!("{}", "Available models:".bold());
    println!();

    for (i, (id, name)) in models.iter().enumerate() {
        println!(
            "  {} {} — {}",
            format!("[{}]", i + 1).dimmed(),
            id.bold().yellow(),
            name
        );
    }

    println!();
    print!("{} Select model (1-{}): ", "→".cyan(), models.len());
    io::stdout().flush().ok()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok()?;
    let num: usize = input.trim().parse().ok()?;

    if (1..=models.len()).contains(&num) {
        Some(models[num - 1].0.clone())
    } else {
        None
    }
}

/// Client-facing base URL for the proxy.
///
/// `0.0.0.0` and `[::]` are bind-only addresses: handing either to Claude Code
/// as its base URL makes the connection fail on some platforms.
fn client_base_url(listen: &str) -> String {
    match resolve_listen_addr(listen) {
        Ok(addr) => {
            let mut client_addr = addr;
            if client_addr.ip().is_unspecified() {
                client_addr.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
            }
            format!("http://{}", client_addr)
        }
        Err(_) => format!("http://{}", listen),
    }
}

/// Block until the proxy answers on `/health`, or time out.
fn wait_for_proxy(base_url: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let url = format!("{}/health", base_url.trim_end_matches('/'));

    while std::time::Instant::now() < deadline {
        if reqwest::blocking::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

fn check_opencode_running(base_url: &str) -> bool {
    let url = detect::endpoint(base_url, "/v1/models");
    reqwest::blocking::Client::builder()
        .user_agent(detect::USER_AGENT)
        .build()
        .map(|client| {
            client
                .get(&url)
                .timeout(Duration::from_secs(5))
                .header(
                    "Authorization",
                    format!("Bearer {}", detect::DEFAULT_OPENCODE_API_KEY),
                )
                .send()
                .map(|r| r.status().is_success())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Automatic discovery helpers
// ---------------------------------------------------------------------------

/// Default proxy port. Anything already sitting on it makes `ocinfinity`
/// climb to the next free port instead of failing.
const DEFAULT_PROXY_PORT: u16 = 8080;

/// Pick a listen address: the one the operator asked for, or the first free
/// port from [`DEFAULT_PROXY_PORT`] upwards.
fn resolve_listen(listen: Option<&String>) -> String {
    if let Some(explicit) = listen {
        return explicit.clone();
    }

    let port = detect::free_port(DEFAULT_PROXY_PORT);
    if port != DEFAULT_PROXY_PORT {
        eprintln!(
            "{} port {} is busy; using {}",
            "[INFO]".yellow(),
            DEFAULT_PROXY_PORT,
            port
        );
    }
    format!("127.0.0.1:{}", port)
}

/// Resolve the OpenCode Zen upstream for a command.
///
/// An explicit `--upstream` always wins. Otherwise a saved configuration value
/// is used only while it still answers, then live port detection, then the
/// conventional default. The second element reports how the value was chosen.
fn resolve_upstream(explicit: Option<&String>) -> (String, &'static str) {
    let config = load_config();
    detect::resolve_upstream(explicit.map(String::as_str), Some(config.upstream.as_str()))
}

/// Print the discovery summary shared by `detect` and `doctor`.
fn render_snapshot(verbose_detail: bool) {
    let services = detect::snapshot();
    if services.is_empty() {
        println!("{} nothing detected", "[WARN]".yellow());
        return;
    }

    for service in services {
        let mut line = format!("{} {}", "→".cyan(), service.name.bold());
        if service.port != 0 {
            line.push_str(&format!(" :{}", service.port.to_string().yellow()));
        }
        if !service.url.is_empty() && service.port != 0 {
            line.push_str(&format!(" {}", service.url.dimmed()));
        }
        if verbose_detail && !service.detail.is_empty() {
            line.push_str(&format!(" — {}", service.detail.dimmed()));
        }
        println!("{}", line);
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    platform::enable_utf8_console();

    let cli = Cli::parse();

    if cli.verbose {
        eprintln!("{} verbose mode enabled", "[DEBUG]".yellow());
    }
    if cli.dry_run {
        eprintln!("{} dry run: no changes will be made", "[DRY-RUN]".cyan());
    }

    let warp_installed = platform::is_warp_installed();

    let exit_code = match &cli.command {
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "ocinfinity", &mut std::io::stdout());
            0
        }

        Commands::Models { upstream, api_key } => {
            let (upstream, source) = resolve_upstream(upstream.as_ref());

            print_banner();
            println!("{}", "Available Models".bold().cyan());
            println!();
            println!(
                "{} fetching models from {} ({})...",
                "→".cyan(),
                upstream.bold(),
                source.dimmed()
            );
            println!();

            match fetch_models(&upstream, api_key) {
                Ok(models) if models.is_empty() => {
                    println!("{} upstream returned no models", "[WARN]".yellow());
                    1
                }
                Ok(models) => {
                    println!("  {}  {:<34}  {}", "#".dimmed(), "ID".bold(), "Name".bold());
                    println!("  {}", "─".repeat(62).dimmed());

                    for (i, (id, name)) in models.iter().enumerate() {
                        println!(
                            "  {}  {:<34}  {}",
                            format!("[{}]", i + 1).dimmed(),
                            id.yellow(),
                            name
                        );
                    }

                    println!();
                    println!("{}", "Usage:".bold());
                    println!(
                        "  {}",
                        "ocinfinity connect claude-code --model <id>"
                            .bold()
                            .yellow()
                    );
                    println!();
                    println!("{}", "Examples:".bold());
                    for (id, _) in models.iter().take(3) {
                        println!(
                            "  {}",
                            format!("ocinfinity connect claude-code --model {}", id).dimmed()
                        );
                    }
                    0
                }
                Err(e) => {
                    println!("{} failed to fetch models: {}", "[ERROR]".red(), e);
                    println!(
                        "{} is OpenCode Zen running at {}?",
                        "Tip:".cyan(),
                        upstream.bold()
                    );
                    1
                }
            }
        }

        Commands::Setup {} => {
            print_banner();
            println!("{}", "Welcome to ocinfinity Setup".bold().green());
            println!();
            println!(
                "{}",
                "This checks and installs every required component.".dimmed()
            );
            println!();

            let mut needed: Vec<(&str, &str, &str)> = Vec::new();
            if !tool_exists("node") {
                needed.push(("Node.js", "manual", "Required for Claude Code and OpenCode"));
            }
            if !warp_installed {
                needed.push(("Cloudflare WARP", "warp", "Required for rate limit reset"));
            }
            if !tool_exists("opencode") {
                needed.push(("OpenCode", "opencode", "AI coding assistant backend"));
            }
            if !tool_exists("claude") {
                needed.push(("Claude Code", "claude-code", "AI coding assistant CLI"));
            }

            if needed.is_empty() {
                println!("{}", "Everything is already installed!".green().bold());
                println!();
                println!("{} run to get started:", "Tip:".cyan().bold());
                println!("  {}", "ocinfinity connect claude-code".bold().yellow());
                return;
            }

            println!("{}", "The following need to be installed:".bold());
            println!();
            for (name, _, desc) in &needed {
                print_info_formatted(name, desc);
            }

            let total = needed.len();
            for (i, (name, installer, _)) in needed.iter().enumerate() {
                println!();
                print_step(i + 1, total, &format!("Installing {}...", name));

                let result = match *installer {
                    "warp" => run_self(&["install", "warp"]),
                    "opencode" => run_self(&["install", "opencode"]),
                    "claude-code" => run_self(&["install", "claude-code"]),
                    // Node.js has no safe cross-platform silent installer.
                    _ => {
                        println!("  {} install Node.js manually:", "ℹ".yellow());
                        println!("    {}", "https://nodejs.org/".bold().yellow());
                        println!("    {}", "or: nvm install --lts".dimmed());
                        Ok(())
                    }
                };

                match result {
                    Ok(()) => print_success(&format!("{} ready", name)),
                    Err(e) => print_fail(&format!("{}: {}", name, e)),
                }
            }

            println!();
            println!("{}", "Setup finished.".green().bold());
            println!();
            println!("{}", "Next steps:".bold());
            println!("  1. {}", "ocinfinity doctor".bold().yellow());
            println!("  2. {}", "ocinfinity connect claude-code".bold().yellow());
            0
        }

        Commands::Install { target } => match target {
            InstallTarget::Warp {} => install_warp(cli.dry_run, cli.verbose, warp_installed),

            InstallTarget::Opencode {} => {
                print_banner();
                println!("{}", "Installing OpenCode v2".bold().green());
                println!();

                // v1 shipped as `opencode-ai`; v2 ships as `@opencode/cli`.
                // Both expose the same `opencode` command, so a stale v1
                // install must be replaced rather than treated as "installed".
                let legacy_v1 = npm_package_installed("opencode-ai");

                if legacy_v1 {
                    println!(
                        "{} legacy OpenCode v1 ({}) detected — replacing it with v2",
                        "[WARN]".yellow(),
                        "opencode-ai".dimmed()
                    );
                    println!(
                        "{} {}",
                        "Migration notes:".cyan(),
                        "https://opencode.ai/v2/docs/migrate-v1".dimmed()
                    );
                    println!();
                } else if tool_exists("opencode") {
                    let version = run_command("opencode", &["--version"])
                        .unwrap_or_else(|_| "unknown".to_string());
                    println!(
                        "{} OpenCode is already installed ({})",
                        "[INFO]".green(),
                        version.dimmed()
                    );
                    println!(
                        "{} to force a reinstall: {}",
                        "Tip:".cyan(),
                        "npm install -g @opencode/cli".dimmed()
                    );
                    return;
                }

                if !tool_exists("node") {
                    println!("{} Node.js is required but not found", "[ERROR]".red());
                    println!("{} https://nodejs.org/", "Fix:".cyan());
                    std::process::exit(1);
                }

                print_step(1, 2, "Installing OpenCode v2 via npm...");
                match run_interactive("npm", &["install", "-g", "@opencode/cli"]) {
                    Ok(()) => {
                        print_success("OpenCode v2 installed");

                        print_step(2, 2, "Verifying the installation...");
                        match run_command("opencode", &["--version"]) {
                            Ok(version) => print_success(&format!("opencode {}", version)),
                            Err(e) => println!(
                                "  {} could not read the version: {}",
                                "⚠".yellow(),
                                e.dimmed()
                            ),
                        }

                        println!();
                        println!("{}", "Next steps:".bold());
                        println!("  1. {}", "opencode".bold().yellow());
                        println!("  2. {}", "ocinfinity doctor".bold().yellow());
                        0
                    }
                    Err(e) => {
                        print_fail(&format!("installation failed: {}", e));
                        println!();
                        println!("{} try manually:", "Fix:".cyan());
                        println!("  {}", "npm install -g @opencode/cli".bold().yellow());
                        1
                    }
                }
            }

            InstallTarget::ClaudeCode {} => {
                print_banner();
                println!("{}", "Installing Claude Code".bold().green());
                println!();

                if tool_exists("claude") {
                    let version = run_command("claude", &["--version"])
                        .unwrap_or_else(|_| "unknown".to_string());
                    println!(
                        "{} Claude Code is already installed ({})",
                        "[INFO]".green(),
                        version.dimmed()
                    );
                    println!("{} checking for a newer release...", "→".cyan());
                    println!();
                }
                if !tool_exists("node") {
                    println!("{} Node.js is required but not found", "[ERROR]".red());
                    println!("{} https://nodejs.org/", "Fix:".cyan());
                    std::process::exit(1);
                }

                print_step(1, 1, "Installing the latest Claude Code via npm...");
                match run_interactive(
                    "npm",
                    &["install", "-g", "@anthropic-ai/claude-code@latest"],
                ) {
                    Ok(()) => {
                        print_success("Claude Code is up to date");
                        println!();
                        println!(
                            "{} {}",
                            "Version:".bold(),
                            run_command("claude", &["--version"])
                                .unwrap_or_else(|_| "unknown".to_string())
                                .dimmed()
                        );
                        println!();
                        println!("{}", "Next steps:".bold());
                        println!("  1. {}", "claude --version".bold().yellow());
                        println!("  2. {}", "ocinfinity connect claude-code".bold().yellow());
                        0
                    }
                    Err(e) => {
                        print_fail(&format!("installation failed: {}", e));
                        println!();
                        println!("{} try manually:", "Fix:".cyan());
                        println!(
                            "  {}",
                            "npm install -g @anthropic-ai/claude-code@latest"
                                .bold()
                                .yellow()
                        );
                        1
                    }
                }
            }

            InstallTarget::All {} => {
                print_banner();
                println!("{}", "Installing all components".bold().green());
                println!();

                let total = 3;
                let mut failed = 0usize;

                print_step(1, total, "Checking Cloudflare WARP...");
                if warp_installed {
                    print_success("WARP already installed");
                } else if let Err(e) = run_self(&["install", "warp"]) {
                    print_fail(&e);
                    failed += 1;
                }

                print_step(2, total, "Checking OpenCode...");
                if tool_exists("opencode") {
                    print_success("OpenCode already installed");
                } else if let Err(e) = run_self(&["install", "opencode"]) {
                    print_fail(&e);
                    failed += 1;
                }

                print_step(3, total, "Checking Claude Code...");
                if tool_exists("claude") {
                    print_success("Claude Code already installed");
                } else if let Err(e) = run_self(&["install", "claude-code"]) {
                    print_fail(&e);
                    failed += 1;
                }

                println!();
                if failed == 0 {
                    println!("{}", "All components installed!".green().bold());
                    println!();
                    println!("{}", "Run to get started:".bold());
                    println!("  {}", "ocinfinity connect claude-code".bold().yellow());
                    0
                } else {
                    println!(
                        "{} {} component(s) failed to install",
                        "[WARN]".yellow(),
                        failed
                    );
                    1
                }
            }
        },

        Commands::Connect {
            target:
                ConnectTarget::ClaudeCode {
                    listen,
                    upstream,
                    api_key,
                    max_retries,
                    warp_delay,
                    model,
                    system_prompt,
                    claude_args,
                },
        } => {
            print_banner();
            println!("{}", "Claude Code Bridge".bold().green());
            println!();

            if !tool_exists("claude") {
                eprintln!("{} Claude Code not found in PATH", "[ERROR]".red());
                eprintln!(
                    "{} {}",
                    "Fix:".cyan(),
                    "ocinfinity install claude-code".bold().yellow()
                );
                std::process::exit(1);
            }

            let listen_requested = listen.is_some();
            let listen = resolve_listen(listen.as_ref());
            let (upstream, upstream_source) = resolve_upstream(upstream.as_ref());

            if let Err(e) = resolve_listen_addr(&listen) {
                eprintln!("{} {}", "[ERROR]".red(), e);
                std::process::exit(1);
            }

            // Reuse an already-running bridge when the operator did not ask for
            // a specific port: re-binding would fail and spawning a second copy
            // would fight the first one over WARP resets.
            let existing_proxy = if listen_requested {
                None
            } else {
                detect::detect_running_proxy()
            };

            let base_url = match &existing_proxy {
                Some(proxy) => proxy.url.clone(),
                None => client_base_url(&listen),
            };

            let selected_model = match model {
                Some(m) => m.clone(),
                None => {
                    println!(
                        "{} fetching models from {} ({})...",
                        "→".cyan(),
                        upstream.bold(),
                        upstream_source.dimmed()
                    );
                    match fetch_models(&upstream, api_key) {
                        Ok(models) if models.is_empty() => {
                            println!("{} no models found; using the default", "[WARN]".yellow());
                            String::new()
                        }
                        Ok(models) => match select_model_interactively(&models) {
                            Some(id) => id,
                            None => {
                                println!(
                                    "{} no selection made; using the default",
                                    "[WARN]".yellow()
                                );
                                String::new()
                            }
                        },
                        Err(e) => {
                            println!("{} failed to fetch models: {}", "[WARN]".yellow(), e);
                            println!("{} starting without a model selection", "Tip:".cyan());
                            String::new()
                        }
                    }
                }
            };

            println!();
            println!(
                "{} proxy:      {}{}",
                "→".green(),
                base_url.bold(),
                if existing_proxy.is_some() {
                    " (already running)".dimmed().to_string()
                } else {
                    String::new()
                }
            );
            println!(
                "{} upstream:   {} ({})",
                "→".green(),
                upstream.bold(),
                upstream_source.dimmed()
            );
            println!(
                "{} auto-reset: {} (attempts: {})",
                "→".green(),
                "enabled".green().bold(),
                max_retries.to_string().bold()
            );
            if !selected_model.is_empty() {
                println!("{} model:      {}", "→".green(), selected_model.bold());
            }
            println!();

            if cli.dry_run {
                println!(
                    "{}",
                    format!(
                        "[DRY-RUN] would proxy {} -> {} and launch claude",
                        base_url, upstream
                    )
                    .cyan()
                );
                return;
            }

            init_tracing(cli.verbose);

            let mut proxy_handle = None;

            if existing_proxy.is_none() {
                let config = ProxyConfig {
                    listen_addr: listen.clone(),
                    opencode_base_url: upstream.clone(),
                    opencode_api_key: api_key.clone(),
                    max_retries: *max_retries,
                    warp_reset_delay_ms: *warp_delay,
                };

                println!("{}", "Starting proxy server...".dimmed());

                let proxy_config = config.clone();
                proxy_handle = Some(std::thread::spawn(move || {
                    let rt = match tokio::runtime::Runtime::new() {
                        Ok(rt) => rt,
                        Err(e) => return Err(anyhow::anyhow!("failed to build runtime: {}", e)),
                    };
                    rt.block_on(ocinfinity::proxy::start_proxy_server(proxy_config))
                }));

                if !wait_for_proxy(&base_url, Duration::from_secs(10)) {
                    eprintln!(
                        "{} proxy did not come up on {}",
                        "[ERROR]".red(),
                        base_url.bold()
                    );
                    if probe_port(&listen) {
                        eprintln!(
                            "{} nothing is listening on {}; check the log above",
                            "Tip:".cyan(),
                            listen.bold()
                        );
                    } else {
                        eprintln!(
                            "{} port is unavailable — is another ocinfinity instance running?",
                            "Tip:".cyan()
                        );
                    }
                    std::process::exit(1);
                }
            }

            println!("{}", "Launching Claude Code...".dimmed());
            println!();

            // Route through `cmd.exe` when `claude` is an npm `.cmd` shim;
            // CreateProcess cannot execute those directly.
            let mut cmd = match resolve_tool("claude") {
                Some((path, true)) => {
                    let mut c = Command::new("cmd");
                    c.arg("/c").arg(&path);
                    c
                }
                Some((path, false)) => Command::new(&path),
                None => {
                    eprintln!("{} Claude Code not found in PATH", "[ERROR]".red());
                    std::process::exit(1);
                }
            };

            // The proxy does not authenticate clients, so hand Claude Code a
            // placeholder key rather than leaking the upstream credential.
            cmd.env("ANTHROPIC_BASE_URL", &base_url)
                .env("ANTHROPIC_API_KEY", "ocinfinity-proxy-key");

            if !selected_model.is_empty() {
                cmd.env("ANTHROPIC_MODEL", &selected_model);
            }
            if let Some(sp) = system_prompt {
                cmd.env("CLAUDE_CODE_SYSTEM_PROMPT", sp);
            }
            if !claude_args.is_empty() {
                cmd.args(claude_args);
            }

            let status_text = match cmd.status() {
                Ok(status) => status.to_string(),
                Err(e) => {
                    eprintln!("{} failed to launch Claude Code: {}", "[ERROR]".red(), e);
                    std::process::exit(1);
                }
            };

            println!();
            println!(
                "{} Claude Code exited with: {}",
                "Info:".cyan(),
                status_text.bold()
            );

            // Dropping the handle detaches the proxy thread; the runtime exits
            // with the process, which is the behaviour we want here.
            if let Some(handle) = proxy_handle {
                println!("{} shutting down proxy...", "→".green().dimmed());
                drop(handle);
            }
            0
        }

        Commands::Watch {
            upstream,
            api_key,
            max_retries,
            warp_delay,
        } => {
            let (upstream, upstream_source) = resolve_upstream(upstream.as_ref());

            print_banner();
            println!("{}", "OpenCode Watch Mode".bold().yellow());
            println!();
            println!(
                "{} monitoring: {} ({})",
                "→".green(),
                upstream.bold(),
                upstream_source.dimmed()
            );
            println!(
                "{} auto-reset: {} (attempts: {})",
                "→".green(),
                "enabled".green().bold(),
                max_retries.to_string().bold()
            );
            println!();

            init_tracing(cli.verbose);

            let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
            let result = rt.block_on(ocinfinity::watch::start_watch_mode(
                &upstream,
                api_key,
                *max_retries,
                *warp_delay,
            ));
            if let Err(e) = result {
                eprintln!("{} watch error: {}", "[ERROR]".red(), e);
                1
            } else {
                0
            }
        }

        Commands::Daemon {
            listen,
            upstream,
            api_key,
            max_retries,
            warp_delay,
        } => {
            let listen = resolve_listen(listen.as_ref());
            let (upstream, upstream_source) = resolve_upstream(upstream.as_ref());

            print_banner();
            println!("{}", "Daemon Mode".bold().magenta());
            println!();
            println!("{} listening:  {}", "→".green(), listen.bold());
            println!(
                "{} upstream:   {} ({})",
                "→".green(),
                upstream.bold(),
                upstream_source.dimmed()
            );
            println!(
                "{} auto-reset: {} (attempts: {})",
                "→".green(),
                "enabled".green().bold(),
                max_retries.to_string().bold()
            );
            println!();
            println!(
                "{} rate limits are recovered silently in the background",
                "Tip:".cyan()
            );
            println!();

            let config = ProxyConfig {
                listen_addr: listen.clone(),
                opencode_base_url: upstream.clone(),
                opencode_api_key: api_key.clone(),
                max_retries: *max_retries,
                warp_reset_delay_ms: *warp_delay,
            };

            init_tracing(cli.verbose);

            let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
            match rt.block_on(ocinfinity::proxy::start_proxy_server(config)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{} daemon error: {}", "[ERROR]".red(), e);
                    1
                }
            }
        }

        Commands::Proxy {
            listen,
            upstream,
            api_key,
            max_retries,
            warp_delay,
        } => {
            let listen = resolve_listen(listen.as_ref());
            let (upstream, upstream_source) = resolve_upstream(upstream.as_ref());

            if let Err(e) = resolve_listen_addr(&listen) {
                eprintln!("{} {}", "[ERROR]".red(), e);
                std::process::exit(1);
            }

            let config = ProxyConfig {
                listen_addr: listen.clone(),
                opencode_base_url: upstream.clone(),
                opencode_api_key: api_key.clone(),
                max_retries: *max_retries,
                warp_reset_delay_ms: *warp_delay,
            };

            print_banner();
            println!("{}", "Anthropic ↔ OpenCode Zen Proxy".bold().cyan());
            println!();
            println!("{} listening on: {}", "→".green(), listen.bold());
            println!(
                "{} upstream:     {} ({})",
                "→".green(),
                upstream.bold(),
                upstream_source.dimmed()
            );
            println!(
                "{} max retries:   {}",
                "→".green(),
                max_retries.to_string().bold()
            );
            println!();
            println!(
                "{} point Claude Code at: {}",
                "Tip:".cyan().bold(),
                client_base_url(&listen).yellow().bold()
            );
            println!(
                "{} or run: {}",
                "→".cyan(),
                "ocinfinity connect claude-code".bold()
            );
            println!();

            init_tracing(cli.verbose);

            let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
            match rt.block_on(ocinfinity::proxy::start_proxy_server(config)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{} proxy server error: {}", "[ERROR]".red(), e);
                    1
                }
            }
        }

        Commands::Doctor {} => {
            print_banner();
            println!("{}", "System Diagnostics".bold().cyan());
            println!();

            let mut all_ok = true;
            let mut checks: Vec<(String, String, bool)> = Vec::new();

            checks.push((
                "Platform".to_string(),
                platform::os_label().to_string(),
                true,
            ));

            if warp_installed {
                let version = platform::run_warp_blocking(&["--version"])
                    .unwrap_or_else(|_| "unknown".to_string());
                checks.push((
                    "WARP CLI".to_string(),
                    format!("installed ({})", version),
                    true,
                ));
            } else {
                checks.push(("WARP CLI".to_string(), "NOT FOUND".to_string(), false));
                all_ok = false;
            }

            match run_command("claude", &["--version"]) {
                Ok(version) => checks.push((
                    "Claude Code".to_string(),
                    format!("installed ({})", version),
                    true,
                )),
                Err(_) => {
                    checks.push(("Claude Code".to_string(), "NOT FOUND".to_string(), false));
                    all_ok = false;
                }
            }

            if tool_exists("opencode") {
                let version = run_command("opencode", &["--version"])
                    .unwrap_or_else(|_| "unknown".to_string());
                let legacy = npm_package_installed("opencode-ai");
                checks.push((
                    "OpenCode".to_string(),
                    if legacy {
                        format!("installed ({}), legacy v1 package present", version)
                    } else {
                        format!("installed ({})", version)
                    },
                    true,
                ));
            } else {
                checks.push(("OpenCode".to_string(), "NOT FOUND".to_string(), false));
                all_ok = false;
            }

            // Auto-detect the upstream rather than trusting the saved value
            // blindly: the port OpenCode listens on changes between restarts.
            let (upstream, upstream_source) = resolve_upstream(None);
            if check_opencode_running(&upstream) {
                checks.push((
                    "OpenCode gateway".to_string(),
                    format!("reachable ({}, {})", upstream, upstream_source),
                    true,
                ));
            } else {
                checks.push((
                    "OpenCode gateway".to_string(),
                    format!("NOT REACHABLE ({}, {})", upstream, upstream_source),
                    false,
                ));
                all_ok = false;
            }

            match detect::v2_service_url() {
                Some(url) => checks.push((
                    "OpenCode v2 service".to_string(),
                    format!("running at {}", url),
                    true,
                )),
                None => checks.push((
                    "OpenCode v2 service".to_string(),
                    "not running (start it with `opencode service start`)".to_string(),
                    true,
                )),
            }

            let cfg_path = config_path();
            let config = load_config();
            if cfg_path.exists() {
                checks.push((
                    "Config file".to_string(),
                    format!("found ({})", cfg_path.display()),
                    true,
                ));
            } else {
                checks.push((
                    "Config file".to_string(),
                    "using defaults".to_string(),
                    true,
                ));
            }

            // Check the port the user actually configured, not a hard-coded
            // 8080 that tells them nothing about their real listen address.
            match resolve_listen_addr(&config.listen) {
                Ok(addr) => {
                    if std::net::TcpListener::bind(addr).is_ok() {
                        checks.push((
                            format!("Port {}", addr.port()),
                            "available".to_string(),
                            true,
                        ));
                    } else {
                        checks.push((format!("Port {}", addr.port()), "IN USE".to_string(), false));
                        all_ok = false;
                    }
                }
                Err(e) => {
                    checks.push((
                        "Listen address".to_string(),
                        format!("INVALID ({})", e),
                        false,
                    ));
                    all_ok = false;
                }
            }

            if let Some(proxy) = detect::detect_running_proxy() {
                checks.push((
                    "Running proxy".to_string(),
                    format!("serving on {} (reusable)", proxy.url),
                    true,
                ));
            }

            for (name, status, ok) in &checks {
                let status_str = if *ok {
                    status.green().bold()
                } else {
                    status.red().bold()
                };
                println!("{} {}: {}", "→".cyan(), name.bold(), status_str);
            }

            println!();
            if all_ok {
                println!("{}", "All checks passed.".green().bold());
            } else {
                println!("{}", "Some checks failed.".yellow().bold());
                println!();
                println!(
                    "{} run `ocinfinity setup` for guided installation",
                    "Fix:".cyan()
                );
            }
            if all_ok {
                0
            } else {
                1
            }
        }

        Commands::Detect { json } => {
            let services = detect::snapshot();
            let (upstream, upstream_source) = resolve_upstream(None);
            let listen = resolve_listen(None);

            if *json {
                let entries: Vec<serde_json::Value> = services
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "url": s.url,
                            "port": s.port,
                            "detail": s.detail,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "services": entries,
                        "upstream": upstream,
                        "upstream_source": upstream_source,
                        "listen": listen,
                    })
                );
                return;
            }

            print_banner();
            println!("{}", "Service Discovery".bold().cyan());
            println!();
            render_snapshot(true);
            println!();
            println!(
                "{} selected upstream: {} ({})",
                "→".green(),
                upstream.bold(),
                upstream_source.dimmed()
            );
            println!("{} next free listen: {}", "→".green(), listen.bold());
            0
        }

        Commands::Config { action } => match action {
            ConfigAction::Show {} => {
                let config = load_config();
                println!("{}", "Current Configuration".bold());
                println!();
                println!("{} {}", "Listen:".bold(), config.listen);
                println!("{} {}", "Upstream:".bold(), config.upstream);
                println!(
                    "{} {}",
                    "API key:".bold(),
                    match &config.api_key {
                        Some(k) if !k.is_empty() => "(set)".to_string(),
                        _ => "(none)".to_string(),
                    }
                );
                println!("{} {}", "Max retries:".bold(), config.max_retries);
                println!("{} {}ms", "WARP delay:".bold(), config.warp_delay);
                println!("{} {}", "Config file:".bold(), config_path().display());
                0
            }
            ConfigAction::Set {
                listen,
                upstream,
                api_key,
                max_retries,
                warp_delay,
                clear_api_key,
            } => {
                // Load first, then patch only the keys the user supplied. The
                // previous version reset every unspecified field to its default,
                // so `ocinfinity config set --upstream X` also clobbered the listen
                // address, retry count and delay.
                let mut config = load_config();

                let mut changed: Vec<&str> = Vec::new();
                if let Some(listen) = listen {
                    if let Err(e) = resolve_listen_addr(listen) {
                        eprintln!("{} --listen {}", "[ERROR]".red(), e);
                        std::process::exit(1);
                    }
                    config.listen = listen.clone();
                    changed.push("listen");
                }
                if let Some(upstream) = upstream {
                    config.upstream = upstream.clone();
                    changed.push("upstream");
                }
                if let Some(key) = api_key {
                    config.api_key = if key.is_empty() {
                        None
                    } else {
                        Some(key.clone())
                    };
                    changed.push("api_key");
                }
                if *clear_api_key {
                    config.api_key = None;
                    changed.push("api_key (cleared)");
                }
                if let Some(retries) = max_retries {
                    config.max_retries = *retries;
                    changed.push("max_retries");
                }
                if let Some(delay) = warp_delay {
                    config.warp_delay = *delay;
                    changed.push("warp_delay");
                }

                if changed.is_empty() {
                    println!(
                        "{} nothing to change; pass at least one flag",
                        "[WARN]".yellow()
                    );
                    println!(
                        "  {}",
                        "ocinfinity config set --upstream https://opencode.ai/inference/v1 --listen 127.0.0.1:9000"
                            .dimmed()
                    );
                    return;
                }

                match save_config(&config) {
                    Ok(path) => {
                        println!("{}", "Configuration saved!".green().bold());
                        println!("{} updated: {}", "→".cyan(), changed.join(", "));
                        println!("{} {}", "File:".bold(), path.display());
                        0
                    }
                    Err(e) => {
                        eprintln!("{} failed to save config: {}", "[ERROR]".red(), e);
                        1
                    }
                }
            }
            ConfigAction::Reset {} => {
                let path = config_path();
                if path.exists() {
                    match fs::remove_file(&path) {
                        Ok(()) => {
                            println!("{} Configuration reset to defaults!", "✓".green().bold());
                            0
                        }
                        Err(e) => {
                            eprintln!("{} failed to remove config: {}", "[ERROR]".red(), e);
                            1
                        }
                    }
                } else {
                    println!("{} no configuration file found", "[INFO]".green());
                    0
                }
            }
        },

        Commands::QuickReset {} => {
            if !warp_installed {
                println!("{} WARP is not installed", "[ERROR]".red());
                println!(
                    "{} run `ocinfinity install warp` first",
                    "Tip:".cyan().bold()
                );
                std::process::exit(1);
            }

            println!("{}", "Quick-resetting WARP tunnel...".bold());

            if cli.dry_run {
                println!(
                    "{}",
                    "[DRY-RUN] would disconnect, re-register and reconnect".cyan()
                );
                return;
            }

            let resolver = ocinfinity::warp::WarpResolver::new(1, 0);
            match resolver.quick_reset_blocking() {
                Ok(()) => {
                    println!("{}", "Quick reset complete!".green().bold());
                    0
                }
                Err(e) => {
                    eprintln!("{} quick reset failed: {}", "[ERROR]".red(), e);
                    1
                }
            }
        }

        Commands::Status { json } => {
            if !warp_installed {
                if *json {
                    println!(
                        "{}",
                        r#"{"connected":false,"tunnel_id":null,"error":"warp-cli not installed"}"#
                    );
                } else {
                    println!("{} {}", "Tunnel:".bold(), "Not connected".red());
                    println!("{} {}", "WARP:".bold(), "Not installed".red());
                    println!();
                    println!(
                        "{} run `ocinfinity install warp` first",
                        "Tip:".cyan().bold()
                    );
                }
                std::process::exit(1);
            }

            if !*json {
                println!("{}", "Checking tunnel status...".dimmed());
            }

            match platform::run_warp_blocking(&["status"]) {
                Ok(output) => {
                    let connected = output.to_lowercase().contains("connected")
                        && !output.to_lowercase().contains("disconnected");

                    if *json {
                        // Real JSON only: the banner-free, log-free path keeps
                        // this usable from scripts.
                        println!(
                            "{{\"connected\": {}, \"status\": \"{}\"}}",
                            connected,
                            output.replace('\\', "\\\\").replace('"', "\\\"")
                        );
                    } else {
                        if !output.is_empty() {
                            println!("{}", output);
                        }
                        let tunnel = if connected {
                            "Connected".green()
                        } else {
                            "Not connected".red()
                        };
                        let warp = if connected {
                            "Active".green()
                        } else {
                            "Inactive".red()
                        };
                        println!();
                        println!("{} {}", "Tunnel:".bold(), tunnel);
                        println!("{} {}", "WARP:".bold(), warp);
                    }
                    0
                }
                Err(e) => {
                    if *json {
                        let escaped = e.replace('\\', "\\\\").replace('"', "\\\"");
                        println!(
                            "{{\"connected\": false, \"tunnel_id\": null, \"error\": \"{}\"}}",
                            escaped
                        );
                    } else {
                        println!("{} {}", "Tunnel:".bold(), "Error".red());
                        println!("{} {}", "Status:".bold(), e.red());
                    }
                    1
                }
            }
        }

        Commands::Reset {} => {
            if !warp_installed {
                println!("{} WARP is not installed", "[ERROR]".red());
                println!(
                    "{} run `ocinfinity install warp` first",
                    "Tip:".cyan().bold()
                );
                std::process::exit(1);
            }

            println!("{}", "Resetting WARP tunnel...".bold());

            if cli.dry_run {
                println!(
                    "{}",
                    "[DRY-RUN] would disconnect, restart warp-svc, wipe the registration cache, re-register and reconnect"
                        .cyan()
                );
                return;
            }

            if cli.verbose {
                eprintln!(
                    "{} service control: {}",
                    "[DEBUG]".cyan(),
                    platform::os_label()
                );
            }

            let steps: Vec<(&str, Box<dyn Fn() -> Result<String, String>>)> = vec![
                (
                    "Disconnecting warp-cli",
                    Box::new(|| platform::run_warp_blocking(&["disconnect"])),
                ),
                (
                    "Stopping the WARP service",
                    Box::new(|| platform::control_service(ServiceAction::Stop)),
                ),
                (
                    "Clearing cached registration",
                    Box::new(|| platform::clear_warp_state()),
                ),
                (
                    "Starting the WARP service",
                    Box::new(|| platform::control_service(ServiceAction::Start)),
                ),
                (
                    "Requesting a fresh registration",
                    Box::new(|| platform::reregister_blocking()),
                ),
                (
                    "Reconnecting",
                    Box::new(|| platform::run_warp_blocking(&["connect"])),
                ),
            ];

            let total = steps.len();
            let mut failed: Vec<String> = Vec::new();

            for (i, (label, step)) in steps.iter().enumerate() {
                print_step(i + 1, total, label);
                match step() {
                    Ok(_) => print_success("done"),
                    // A step that is merely inapplicable (service already down,
                    // cache already empty) must not abort the rotation.
                    Err(e) => {
                        println!("  {} {}", "⚠".yellow(), e.dimmed());
                        failed.push(label.to_string());
                    }
                }
            }

            println!();
            // Only the final reconnect is fatal.
            if failed.iter().any(|s| s == "Reconnecting") {
                println!("{} WARP tunnel reset failed", "[ERROR]".red());
                1
            } else {
                println!("{}", "WARP tunnel reset complete!".green().bold());
                if !failed.is_empty() {
                    println!(
                        "{} {} step(s) reported warnings (see above)",
                        "[WARN]".yellow(),
                        failed.len()
                    );
                }
                println!("{} run `ocinfinity status` to verify", "Tip:".cyan());
                0
            }
        }

        Commands::Stop {} => {
            if !warp_installed {
                println!("{} WARP is not installed", "[ERROR]".red());
                println!(
                    "{} run `ocinfinity install warp` first",
                    "Tip:".cyan().bold()
                );
                std::process::exit(1);
            }

            println!("{}", "Stopping WARP tunnel...".bold());

            if cli.dry_run {
                println!(
                    "{}",
                    "[DRY-RUN] would disconnect and disable the WARP service".cyan()
                );
                return;
            }

            let mut ok = true;

            print_step(1, 2, "Disconnecting warp-cli");
            match platform::run_warp_blocking(&["disconnect"]) {
                Ok(_) => print_success("done"),
                Err(e) => {
                    println!("  {} {}", "⚠".yellow(), e.dimmed());
                }
            }

            print_step(2, 2, "Disabling the WARP service");
            match platform::control_service(ServiceAction::Disable) {
                Ok(_) => print_success("done"),
                Err(e) => {
                    print_fail(&e);
                    ok = false;
                }
            }

            println!();
            if ok {
                println!("{}", "WARP tunnel stopped!".green().bold());
                println!("{} run `ocinfinity reset` to restart", "Tip:".cyan());
                0
            } else {
                1
            }
        }

        Commands::About {} => {
            print_banner();
            println!();
            println!("{} {}", "Version:".bold(), VERSION);
            println!("{} Rust ({})", "Platform:".bold(), platform::os_label());
            println!(
                "{} OpenCode limit reset + Claude Code proxy",
                "Purpose:".bold()
            );
            println!("{} Cloudflare WARP + Axum HTTP", "Infrastructure:".bold());
            println!("{} WDRXN", "Author:".bold());
            println!("{} https://github.com/WDRXN/ocinfinity", "GitHub:".bold());
            println!();
            println!("{}", "Installation:".bold());
            println!("  ocinfinity install warp          # Install Cloudflare WARP");
            println!("  ocinfinity install opencode      # Install OpenCode");
            println!("  ocinfinity install claude-code   # Install Claude Code CLI");
            println!("  ocinfinity install all           # Install everything");
            println!("  ocinfinity setup                 # Guided setup wizard");
            println!();
            println!("{}", "WARP Management:".bold());
            println!("  ocinfinity status                # Check WARP status");
            println!("  ocinfinity reset                 # Full WARP tunnel reset");
            println!("  ocinfinity quick-reset           # Fast WARP IP rotation");
            println!("  ocinfinity stop                  # Stop WARP tunnel");
            println!();
            println!("{}", "Proxy & Claude Code:".bold());
            println!("  ocinfinity proxy                 # Start reverse proxy");
            println!("  ocinfinity connect claude-code   # Proxy + launch Claude Code");
            println!("  ocinfinity daemon                # Background proxy service");
            println!("  ocinfinity watch                 # Monitor OpenCode, auto-reset");
            println!();
            println!("{}", "Configuration:".bold());
            println!("  ocinfinity models                # List available OpenCode models");
            println!("  ocinfinity detect                # Detect services and free ports");
            println!("  ocinfinity config show           # Show current config");
            println!("  ocinfinity config set            # Update individual keys");
            println!("  ocinfinity config reset          # Reset to defaults");
            println!("  ocinfinity doctor                # Diagnose system setup");
            println!("  ocinfinity completions <shell>   # Print a shell completion script");
            0
        }
    };

    if !matches!(
        &cli.command,
        Commands::About {}
            | Commands::Proxy { .. }
            | Commands::Connect { .. }
            | Commands::Daemon { .. }
            | Commands::Watch { .. }
            | Commands::Doctor {}
            | Commands::Detect { .. }
            | Commands::Config { .. }
            | Commands::Setup {}
            | Commands::Install { .. }
            | Commands::Models { .. }
            | Commands::Completions { .. }
    ) {
        println!("\n{} v{}", "ocinfinity".bold(), VERSION.dimmed());
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

// ---------------------------------------------------------------------------
// WARP installer
// ---------------------------------------------------------------------------

fn install_warp(dry_run: bool, verbose: bool, warp_installed: bool) -> i32 {
    print_banner();
    println!("{}", "Installing Cloudflare WARP".bold().green());
    println!();

    if warp_installed {
        println!("{} WARP is already installed", "[INFO]".green());
        println!(
            "{} run `ocinfinity reset` to refresh your IP",
            "Tip:".cyan()
        );
        return 0;
    }

    if dry_run {
        println!(
            "{}",
            "[DRY-RUN] would install the Cloudflare WARP client for this platform".cyan()
        );
        return 0;
    }

    if cfg!(windows) {
        install_warp_windows(verbose)
    } else if cfg!(target_os = "linux") {
        install_warp_linux(verbose)
    } else if cfg!(target_os = "macos") {
        install_warp_macos(verbose)
    } else {
        println!("{} unsupported OS. Install manually:", "Warning:".yellow());
        println!(
            "  {}",
            "https://developers.cloudflare.com/warp-client/get-started/".dimmed()
        );
        1
    }
}

#[cfg(windows)]
fn install_warp_windows(verbose: bool) -> i32 {
    // Cloudflare publishes a stable redirector to the current MSI.
    const MSI_URL: &str = "https://downloads.cloudflareclient.com/v1/download/windows/ga";

    // `Start-Process -Verb RunAs` needs a real executable path; bare command
    // names are resolved through the elevated PowerShell process.
    let curl_exe = platform::which("curl.exe")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "curl.exe".to_string());
    let msiexec_exe = platform::which("msiexec.exe")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "msiexec.exe".to_string());

    print_step(1, 2, "Downloading the Cloudflare WARP installer...");

    let msi_path = std::env::temp_dir().join("CloudflareWARP.msi");
    let msi = msi_path
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "CloudflareWARP.msi".to_string());

    if let Err(e) = run_interactive(
        &curl_exe,
        &["-fsSL", "--retry", "3", "-o", msi.as_str(), MSI_URL],
    ) {
        print_fail(&format!("download failed: {}", e));
        println!(
            "{} download it manually from https://1.1.1.1/",
            "Fix:".cyan()
        );
        return 1;
    }
    print_success(&format!("saved to {}", msi_path.display()));

    print_step(2, 2, "Running the installer (approve the UAC prompt)...");
    let install_args = ["/i", msi.as_str(), "/qn", "/norestart"];
    if let Err(_e) = run_elevated_cmd(&msiexec_exe, &install_args, false, verbose) {
        // Silent mode can be blocked by policy; fall back to the UI installer.
        println!(
            "  {} silent install failed; launching the wizard",
            "⚠".yellow()
        );
        let interactive_args = ["/i", msi.as_str()];
        if let Err(e) = run_elevated_cmd(&msiexec_exe, &interactive_args, false, verbose) {
            print_fail(&e);
            return 1;
        }
    }

    println!();
    if platform::is_warp_installed() {
        println!("{}", "Next steps:".bold());
        println!("  1. {}", "warp-cli connect".bold().yellow());
        println!("  2. {}", "warp-cli status".bold().yellow());
        println!("  3. {}", "ocinfinity reset".bold().yellow());
        0
    } else {
        println!(
            "{} install may have failed; open a new shell so PATH refreshes",
            "[WARN]".yellow()
        );
        1
    }
}

#[cfg(not(windows))]
fn install_warp_windows(_verbose: bool) -> i32 {
    unreachable!("windows installer is cfg-gated");
}

fn install_warp_linux(verbose: bool) -> i32 {
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let os = os_release.to_lowercase();

    if os.contains("arch") || os.contains("manjaro") || os.contains("cachyos") {
        print_step(1, 1, "Installing cloudflare-warp via yay...");
        match run_interactive("yay", &["-S", "--noconfirm", "cloudflare-warp-bin"]) {
            Ok(()) => {
                print_success("WARP installed");
                linux_warp_next_steps();
                0
            }
            Err(e) => {
                print_fail(&format!("installation failed: {}", e));
                1
            }
        }
    } else if os.contains("ubuntu") || os.contains("debian") || os.contains("kali") {
        print_step(1, 2, "Adding the Cloudflare repository...");

        // Older ocinfinity releases wrote a sources.list entry that Cloudflare no
        // longer publishes under; remove it before installing the deb-src one.
        let _ = run_elevated_shell(
            "rm -f /etc/apt/sources.list.d/cloudflare-warp.list",
            false,
            verbose,
        );

        if let Err(e) = run_elevated_shell(
            "curl -fsSL https://pkg.cloudflareclient.com/pubkey.gpg | gpg --yes --dearmor --output /usr/share/keyrings/cloudflare-warp-archive-keyring.gpg",
            false,
            verbose,
        ) {
            print_fail(&format!("failed to import the repository key: {}", e));
            return 1;
        }

        // Kali reports `kali-rolling`, which Cloudflare does not publish.
        // Debian stable is the supported substitute on a Debian-derived host.
        let codename = if os.contains("kali") {
            "trixie".to_string()
        } else {
            // Evaluate the codename inside the shell rather than parsing it
            // here, so derivatives that patch VERSION_CODENAME are handled.
            "$(. /etc/os-release && printf '%s' \"$VERSION_CODENAME\")".to_string()
        };

        let repo = format!(
            "echo \"deb [signed-by=/usr/share/keyrings/cloudflare-warp-archive-keyring.gpg] https://pkg.cloudflareclient.com/ {} main\" > /etc/apt/sources.list.d/cloudflare-client.list",
            codename
        );
        if let Err(e) = run_elevated_shell(&repo, false, verbose) {
            print_fail(&format!("failed to write the repository entry: {}", e));
            return 1;
        }

        print_step(2, 2, "Installing cloudflare-warp...");
        match run_elevated_shell(
            "apt-get update -qq && apt-get install -y -qq cloudflare-warp",
            false,
            verbose,
        ) {
            Ok(_) => {
                print_success("WARP installed");
                linux_warp_next_steps();
                0
            }
            Err(e) => {
                print_fail(&format!("installation failed: {}", e));
                1
            }
        }
    } else if os.contains("fedora") || os.contains("rhel") || os.contains("centos") {
        print_step(1, 1, "Installing cloudflare-warp via dnf...");
        match run_elevated_shell("dnf install -y cloudflare-warp", false, verbose) {
            Ok(_) => {
                print_success("WARP installed");
                linux_warp_next_steps();
                0
            }
            Err(e) => {
                print_fail(&format!("installation failed: {}", e));
                1
            }
        }
    } else {
        println!(
            "{} unsupported distribution. Install manually:",
            "Warning:".yellow()
        );
        println!(
            "  {}",
            "https://developers.cloudflare.com/warp-client/get-started/linux/".dimmed()
        );
        1
    }
}

fn linux_warp_next_steps() {
    println!();
    println!("{}", "Next steps:".bold());
    println!("  1. {}", "sudo systemctl start warp-svc".bold().yellow());
    println!("  2. {}", "warp-cli connect".bold().yellow());
    println!("  3. {}", "ocinfinity status".bold().yellow());
}

fn install_warp_macos(verbose: bool) -> i32 {
    print_step(1, 1, "Installing cloudflare-warp via Homebrew...");
    if !tool_exists("brew") {
        print_fail("Homebrew is required but was not found");
        println!("{} https://brew.sh/", "Fix:".cyan());
        return 1;
    }
    match run_interactive("brew", &["install", "--cask", "cloudflare-warp"]) {
        Ok(()) => {
            print_success("WARP installed");
            println!();
            println!("{}", "Next steps:".bold());
            println!(
                "  1. {}",
                "open /Applications/Cloudflare\\ WARP.app".bold().yellow()
            );
            println!("  2. {}", "warp-cli connect".bold().yellow());
            println!("  3. {}", "ocinfinity status".bold().yellow());
            let _ = verbose;
            0
        }
        Err(e) => {
            print_fail(&format!("installation failed: {}", e));
            1
        }
    }
}
