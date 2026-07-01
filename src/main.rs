mod audio;
mod timer;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures::{future, prelude::*};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use ts_bookkeeping::messages::c2s::OutSendTextMessagePart;
use ts_bookkeeping::{ClientId, TextMessageTargetMode};
use tsclientlib::{OutCommandExt, ChannelId, Connection, DisconnectOptions, Identity, StreamItem, events::Event};
use tsproto_packets::packets::OutPacket;

use timer::TimerCommand;

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhisperScope {
    AllChannels,
    CurrentChannel,
}

impl WhisperScope {
    pub fn wire_value(self) -> u8 {
        match self {
            WhisperScope::AllChannels => 0,
            WhisperScope::CurrentChannel => 1,
        }
    }
}

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

#[derive(Debug, Clone)]
pub enum PlaybackTarget {
    CurrentChannel,
    ServerGroup { group_id: u64, scope: WhisperScope },
}

impl PlaybackTarget {
    fn is_whisper(&self) -> bool {
        matches!(self, PlaybackTarget::ServerGroup { .. })
    }

    fn describe(&self) -> String {
        match self {
            PlaybackTarget::CurrentChannel => "current channel".to_string(),
            PlaybackTarget::ServerGroup { group_id, scope } => {
                format!("server group {group_id}, scope={scope:?}")
            }
        }
    }
}

pub struct Inbound {
    pub command: TimerCommand,
    pub from_client: ClientId,
}

#[tokio::main]
async fn main() -> Result<()> {
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
    if !args.warn_60s.exists() {
        bail!(
            "60-second announcement audio file does not exist: {}",
            args.warn_60s.display()
        );
    }
    if !args.warn_30s.exists() {
        bail!(
            "30-second announcement audio file does not exist: {}",
            args.warn_30s.display()
        );
    }
    if !args.warn_15s.exists() {
        bail!(
            "15-second announcement audio file does not exist: {}",
            args.warn_15s.display()
        );
    }

    let target = build_playback_target(&args);

    let mut opts = Connection::build(args.server.clone()).name(args.name.clone());

    if let Some(channel) = &args.channel {
        opts = opts.channel(channel.clone());
    }
    if let Some(channel_id) = args.channel_id {
        opts = opts.channel_id(ChannelId(channel_id));
    }
    if let Some(password) = &args.server_password {
        opts = opts.password(password.clone());
    }
    if let Some(password) = &args.channel_password {
        opts = opts.channel_password(password.clone());
    }
    if let Some(path) = &args.identity_file {
        let identity_text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read identity file {}", path.display()))?;
        let identity = Identity::new_from_str(identity_text.trim())
            .map_err(|e| anyhow!("failed to parse identity from --identity-file: {e:?}"))?;
        opts = opts.identity(identity);
    }

    info!("Connecting to {} as {} ...", args.server, args.name);
    let mut con = opts
        .connect()
        .context("failed to start TeamSpeak connection")?;

    let initial_state = con
        .events()
        .try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
        .next()
        .await
        .transpose()
        .context("failed while waiting for initial TeamSpeak state")?;

    if initial_state.is_none() {
        bail!("connection closed before initial TeamSpeak state arrived");
    }

    info!("Connected. can_send_audio = {}", con.can_send_audio());
    if target.is_whisper() {
        info!("Whisper mode: {}", target.describe());
    } else {
        info!("Normal mode: playing into the bot's current channel");
    }

    info!("Jungle timer is stopped. Send '!jungle start' in TeamSpeak chat to begin.");
    info!("Commands: '!jungle start [MM:SS] [at MM:SS]', '!jungle set MM:SS', '!jungle stop'.");

    let (packet_tx, mut packet_rx) = mpsc::channel::<OutPacket>(64);
    let (timer_tx, timer_rx) = mpsc::channel::<Inbound>(32);
    let (response_tx, mut response_rx) = mpsc::channel::<(ClientId, String)>(32);
    let producer_args = args.clone();
    let producer_target = target.clone();

    tokio::spawn(async move {
        if let Err(err) =
            timer::jungle_timer(producer_args, producer_target, packet_tx, timer_rx, response_tx)
                .await
        {
            error!("Jungle timer stopped: {err:?}");
        }
    });

    loop {
        let command_tx = timer_tx.clone();
        let events = con.events().try_for_each(move |event| {
            let command_tx = command_tx.clone();
            async move {
                handle_stream_item(event, &command_tx).await;
                Ok(())
            }
        });

        tokio::select! {
            maybe_packet = packet_rx.recv() => {
                match maybe_packet {
                    Some(packet) => con.send_audio(packet).context("failed to send TeamSpeak audio packet")?,
                    None => bail!("audio producer exited"),
                }
            }
            result = events => {
                result.context("TeamSpeak event stream ended with an error")?;
                bail!("TeamSpeak connection closed");
            }
            maybe_response = response_rx.recv() => {
                if let Some((client_id, text)) = maybe_response {
                    let msg = OutSendTextMessagePart {
                        target: TextMessageTargetMode::Client,
                        target_client_id: Some(client_id),
                        message: text.into(),
                    };
                    msg.send(&mut con)?;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Disconnecting ...");
                con.disconnect(DisconnectOptions::new())?;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_stream_item(event: StreamItem, command_tx: &mpsc::Sender<Inbound>) {
    let StreamItem::BookEvents(events) = event else {
        return;
    };

    for event in events {
        let Event::Message {
            invoker, message, ..
        } = event
        else {
            continue;
        };

        let Some(parsed) = timer::parse_timer_command(&message) else {
            continue;
        };

        match parsed {
            Ok(command) => {
                info!(client = %invoker.id, command = %message, "command received");
                let inbound = Inbound {
                    command,
                    from_client: invoker.id,
                };
                if command_tx.send(inbound).await.is_err() {
                    error!("could not handle command: timer task stopped");
                }
            }
            Err(err) => {
                warn!(client = %invoker.id, error = %err, "invalid command");
            }
        }
    }
}

fn build_playback_target(args: &Args) -> PlaybackTarget {
    if let Some(group_id) = args.whisper_server_group_id {
        PlaybackTarget::ServerGroup {
            group_id,
            scope: args.whisper_scope,
        }
    } else {
        PlaybackTarget::CurrentChannel
    }
}
