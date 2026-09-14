use clap::{Parser, Subcommand};

mod adapter;
mod api;
mod cli;
mod config;
mod daemon;
mod daemon_nudge;
mod events_bridge;
mod hooks;
mod lock;
mod mpris;
mod paths;
mod qconnect;
mod state;
mod tui;

pub const API_VERSION: u32 = 1; // 02-cli-and-api.md §1.6

/// The version this build reports everywhere (`pibuz version`, `--version`,
/// `/api/status`, the Connect device's softwareVersion). Normally the Cargo
/// version — releases are tagged to match it and CI checks that, so a release
/// build needs no override.
///
/// `PIBUZ_BUILD_ID` exists for the case Cargo.toml cannot express: a binary
/// built straight from a working tree and installed on a Pi, which
/// `scripts/pibuz-to-pi.sh` stamps `2.4.0.local-<sha>-dirty` so it can be
/// identified later. Compile-time (`option_env!`), never read at runtime.
pub const VERSION: &str = match option_env!("PIBUZ_BUILD_ID") {
    Some(id) => id,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "pibuz", version = VERSION, arg_required_else_help = true,
          about = "Pibuz — headless Qobuz Connect daemon")]
struct Cli {
    /// Target daemon (default 127.0.0.1:8182; env QBZD_HOST)
    #[arg(long, global = true)]
    host: Option<String>,
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (systemd ExecStart)
    Run,
    /// Interactive configurator (six screens)
    Setup,
    /// Composite daemon diagnostic
    Status {
        #[arg(long)]
        json: bool,
    },
    Ping {
        #[arg(long)]
        json: bool,
    },
    /// One-line now-playing
    Now {
        #[arg(long)]
        json: bool,
    },
    /// Stream live daemon events (SSE); default = newline-delimited JSON
    Watch {
        #[arg(long)]
        raw: bool,
    },
    /// Shuffle: on | off | toggle (bare = toggle)
    Shuffle {
        mode: Option<String>,
    },
    /// Repeat: off | all | one
    Repeat {
        mode: String,
    },
    /// Resume (bare) or play content: album:ID | track:ID | artist:ID | playlist:ID | URL
    Play {
        content: Option<String>,
    },
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    /// Absolute secs, +N/-N, or mm:ss
    Seek {
        position: String,
    },
    /// Bare = read; 0-100, +N, -N
    Volume {
        value: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bare = toggle
    Mute {
        state: Option<String>,
    },
    Queue {
        #[command(subcommand)]
        cmd: QueueCmd,
    },
    Settings {
        #[command(subcommand)]
        cmd: SettingsCmd,
    },
    Qconnect {
        #[command(subcommand)]
        cmd: QconnectCmd,
    },
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Generate an init service file (systemd/openrc/runit); prints to stdout
    Service {
        /// systemd | openrc | runit (auto-detected from the running init if omitted)
        init: Option<String>,
        /// User the service runs as (default: current user)
        #[arg(long)]
        user: Option<String>,
        /// Path to the pibuz binary (default: this executable, else /usr/bin/pibuz)
        #[arg(long)]
        bin: Option<String>,
        /// systemd: emit a SYSTEM unit (runs as --user) instead of a user unit
        #[arg(long)]
        system: bool,
    },
    /// Shell completions (hidden; packaged by T14)
    #[command(hide = true)]
    Completions {
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum QueueCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Add {
        track_id: u64,
        #[arg(long)]
        next: bool,
    },
    Remove {
        index: usize,
    },
    Clear {
        #[arg(long)]
        keep_current: bool,
    },
    /// Reorder a 1-based position to another
    Move {
        from: usize,
        to: usize,
    },
    /// Jump to (play) a 1-based position
    Jump {
        position: usize,
    },
    /// Stop after the current track (or `off` to clear)
    StopAfter {
        arg: Option<String>,
    },
}

#[derive(Subcommand)]
enum RecoCmd {
    Playlist {
        id: u64,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FavCmd {
    List {
        #[arg(long = "type")]
        kind: Option<String>,
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        json: bool,
    },
    Add {
        fav_type: String,
        id: Option<String>,
        #[arg(long)]
        current: bool,
    },
    Remove {
        fav_type: String,
        id: String,
    },
}

#[derive(Subcommand)]
enum PlaylistCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        id: u64,
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a playlist
    Create {
        name: String,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        public: bool,
    },
    /// Rename / re-describe / change visibility
    Edit {
        id: u64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        public: bool,
        #[arg(long)]
        private: bool,
    },
    /// Delete an owned playlist (requires --yes)
    Rm {
        id: u64,
        #[arg(long)]
        yes: bool,
    },
    /// Add tracks (ids, or - to read from stdin)
    Add { id: u64, track_ids: Vec<String> },
    /// Remove tracks (plain track ids)
    Remove { id: u64, track_ids: Vec<String> },
}

#[derive(Subcommand)]
enum SettingsCmd {
    Export {
        file: Option<String>,
        #[arg(long, default_value = "daemon")]
        from: String, // daemon|desktop
        #[arg(long)]
        include_auth: bool,
    },
    Import {
        file: String,
        #[arg(long)]
        include_auth: bool,
        #[arg(long)]
        remap: Vec<String>, // OLD=NEW, repeatable
        #[arg(long)]
        dry_run: bool,
    },
    Show {
        #[arg(long)]
        json: bool,
    },
    Set {
        key: String,
        value: String,
    },
}

#[derive(Subcommand)]
enum QconnectCmd {
    Enable,
    Disable,
    Name { name: String },
}

// The tokenless default has no rotation verb (02 §3.1.2): `config` is just
// path|show. Rotating the opt-in [server] token = edit pibuz.toml + restart.
#[derive(Subcommand)]
enum ConfigCmd {
    Path,
    Show {
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    // Install the rustls process-level `CryptoProvider`
    qbz_app::ensure_crypto_provider();

    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Version { json } => {
            if json {
                println!(
                    "{{\"version\":\"{}\",\"api_version\":{}}}",
                    VERSION, API_VERSION
                );
            } else {
                println!("Pibuz {} (api v{})", VERSION, API_VERSION);
            }
            0
        }
        Cmd::Service {
            init,
            user,
            bin,
            system,
        } => cli::service::service(init, user, bin, system),
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "pibuz", &mut std::io::stdout());
            0
        }
        Cmd::Run => {
            // Phase 1: resolve the config root and load pibuz.toml. The config's
            // `data_root` (a container override) can redirect the data/cache
            // roots, so resolve those in phase 2 once it is known.
            let bootstrap = paths::ProfileRoots::resolve(None, None);
            let cfg_path = bootstrap.config_file();
            let (cfg, warns) = match config::PibuzConfig::load(&cfg_path) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error: {e}");
                    eprintln!("  → fix or remove the config:  {}", cfg_path.display());
                    std::process::exit(1);
                }
            };
            // Phase 2: honor an explicit `data_root` container override.
            let data_root = cfg.data_root.clone();
            let roots =
                paths::ProfileRoots::resolve(None, data_root.as_deref().map(std::path::Path::new));
            match daemon::run(roots, cfg, warns).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Cmd::Status { json } => {
            // The CLI reads only local pibuz.toml (for the opt-in token); the
            // config root is always at its XDG default.
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::status::status(cli.host, json, &roots).await
        }
        Cmd::Ping { json } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::status::ping(cli.host, json, &roots).await
        }
        Cmd::Now { json } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::now(cli.host, json, &roots).await
        }
        Cmd::Watch { raw } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::watch::watch(cli.host, raw, &roots).await
        }
        Cmd::Shuffle { mode } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::mode::shuffle(cli.host, mode, &roots).await
        }
        Cmd::Repeat { mode } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::mode::repeat(cli.host, mode, &roots).await
        }
        Cmd::Play { content } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::play::play(cli.host, content, &roots).await
        }
        Cmd::Pause => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::pause(cli.host, &roots).await
        }
        Cmd::Toggle => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::toggle(cli.host, &roots).await
        }
        Cmd::Stop => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::stop(cli.host, &roots).await
        }
        Cmd::Next => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::next(cli.host, &roots).await
        }
        Cmd::Prev => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::prev(cli.host, &roots).await
        }
        Cmd::Seek { position } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::seek(cli.host, &roots, position).await
        }
        Cmd::Volume { value, json } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::volume(cli.host, &roots, value, json).await
        }
        Cmd::Mute { state } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            cli::transport::mute(cli.host, &roots, state).await
        }
        Cmd::Queue { cmd } => {
            let roots = paths::ProfileRoots::resolve(None, None);
            match cmd {
                QueueCmd::List { json } => cli::queue::list(cli.host, json, &roots).await,
                QueueCmd::Add { track_id, next } => {
                    cli::queue::add(cli.host, &roots, track_id, next).await
                }
                QueueCmd::Remove { index } => cli::queue::remove(cli.host, &roots, index).await,
                QueueCmd::Clear { keep_current } => {
                    cli::queue::clear(cli.host, &roots, keep_current).await
                }
                QueueCmd::Move { from, to } => cli::queue::move_(cli.host, &roots, from, to).await,
                QueueCmd::Jump { position } => cli::queue::jump(cli.host, &roots, position).await,
                QueueCmd::StopAfter { arg } => cli::queue::stop_after(cli.host, &roots, arg).await,
            }
        }

        Cmd::Settings { cmd } => {
            let roots = login_roots();
            match cmd {
                SettingsCmd::Show { json } => cli::settings::show(json, &roots),
                SettingsCmd::Set { key, value } => cli::settings::set(&roots, &key, &value),
                SettingsCmd::Export {
                    file,
                    from,
                    include_auth,
                } => cli::settings::export(&roots, file, &from, include_auth),
                SettingsCmd::Import {
                    file,
                    include_auth,
                    remap,
                    dry_run,
                } => cli::settings::import(&roots, &file, include_auth, &remap, dry_run).await,
            }
        }
        Cmd::Qconnect { cmd } => {
            let roots = login_roots();
            match cmd {
                QconnectCmd::Enable => cli::settings::qconnect_enable(&roots),
                QconnectCmd::Disable => cli::settings::qconnect_disable(&roots),
                QconnectCmd::Name { name } => cli::settings::qconnect_name(&roots, &name),
            }
        }
        Cmd::Config { cmd } => {
            let roots = login_roots();
            match cmd {
                ConfigCmd::Path => cli::settings::config_path(&roots),
                ConfigCmd::Show { json } => cli::settings::config_show(json, &roots),
            }
        }
        Cmd::Setup => {
            // The setup TUI edits the daemon's REAL stores at the daemon roots,
            // honoring a `pibuz.toml` `data_root` override exactly like `run`.
            let roots = login_roots();
            tui::run(roots).await
        }
    };
    std::process::exit(code);
}

/// Resolve the daemon profile roots for a local CLI auth operation. `login` and
/// `logout` write the credential file into the config root and nudge the LOCAL
/// daemon, so — like `run` — they honor a `pibuz.toml` `data_root` override while
/// keeping the config root at its XDG default.
fn login_roots() -> paths::ProfileRoots {
    let bootstrap = paths::ProfileRoots::resolve(None, None);
    let cfg_path = bootstrap.config_file();
    let data_root = config::PibuzConfig::load(&cfg_path)
        .ok()
        .and_then(|(c, _)| c.data_root);
    paths::ProfileRoots::resolve(None, data_root.as_deref().map(std::path::Path::new))
}
