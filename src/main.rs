// GUI subsystem on Windows: double-clicking the exe must not open a console
// window. CLI mode reattaches to the parent console in main() instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod config;
mod gui;
mod timer;
mod ts3;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tsclientlib::Identity;

use ts3::WhisperScope;

#[derive(Parser, Debug, Clone)]
#[command(
    author,
    version,
    about = "TeamSpeak jungle timer bot for Where Winds Meet guild wars"
)]
pub struct Args {
    #[arg(long)]
    pub server: String,

    #[arg(long, default_value = "assets/Jungle 60 sec.wav")]
    pub warn_60s: PathBuf,

    #[arg(long, default_value = "assets/Jungle 30 sec.wav")]
    pub warn_30s: PathBuf,

    #[arg(long, default_value = "assets/Jungle 15 sec.wav")]
    pub warn_15s: PathBuf,

    #[arg(long, default_value = "rust-mp3-bot")]
    pub name: String,

    #[arg(long)]
    pub channel: Option<String>,

    #[arg(long)]
    pub channel_id: Option<u64>,

    #[arg(long)]
    pub server_password: Option<String>,

    #[arg(long)]
    pub channel_password: Option<String>,

    #[arg(long)]
    pub identity_file: Option<PathBuf>,

    #[arg(long, default_value_t = 1.0)]
    pub volume: f32,

    #[arg(long = "whisper-server-group-id")]
    pub whisper_server_group_id: Option<u64>,

    #[arg(long = "whisper-scope", value_enum, default_value = "all-channels")]
    pub whisper_scope: WhisperScope,
}

fn main() -> Result<()> {
    // When started from a terminal on Windows, reattach to its console so
    // CLI output still shows despite the GUI subsystem above.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    // No arguments (a double-clicked executable) opens the GUI; any argument
    // means the classic headless CLI.
    if std::env::args_os().len() <= 1 {
        gui::run()
    } else {
        run_cli()
    }
}

#[tokio::main]
async fn run_cli() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wwmts3gw=info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    if args.channel.is_some() && args.channel_id.is_some() {
        bail!("use either --channel or --channel-id, not both");
    }
    if args.volume < 0.0 {
        bail!("--volume must be >= 0.0");
    }

    let identity = match &args.identity_file {
        Some(path) => {
            let identity_text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read identity file {}", path.display()))?;
            Some(
                Identity::new_from_str(identity_text.trim()).map_err(|e| {
                    anyhow!("failed to parse identity from --identity-file: {e:?}")
                })?,
            )
        }
        None => None,
    };

    // Decode the clips up front so bad files fail at startup, not mid-game.
    let clip_paths = [&args.warn_60s, &args.warn_30s, &args.warn_15s];
    let mut clips = Vec::with_capacity(clip_paths.len());
    for (&offset, path) in timer::ANNOUNCE_OFFSETS.iter().zip(clip_paths) {
        let pcm = audio::decode_clip(path, args.volume)
            .with_context(|| format!("failed to load {offset}s announcement {}", path.display()))?;
        clips.push((offset, pcm));
    }

    ts3::run(&args, identity, &clips, None).await
}
