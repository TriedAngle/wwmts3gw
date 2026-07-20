mod audio;
mod timer;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures::{future, prelude::*};
use std::path::PathBuf;
use tokio::time::{self, Instant};
use tracing::{info, warn};
use ts_bookkeeping::messages::c2s::OutSendTextMessagePart;
use ts_bookkeeping::TextMessageTargetMode;
use tsclientlib::{OutCommandExt, ChannelId, Connection, DisconnectOptions, Identity, StreamItem, events::Event};

use timer::{TimerCommand, TimerState};

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

/// An announcement clip mid-stream: precomputed Opus frames, sent one per tick.
struct Playback<'a> {
    frames: &'a [Vec<u8>],
    idx: usize,
    next_at: Instant,
}

/// What the next deadline does when it fires.
#[derive(Clone, Copy)]
enum Due {
    Frame,
    GameStart { game_zero: Instant },
    Announce { offset: u64, game_zero: Instant },
}

/// TeamSpeak clients render ':x' sequences as emoticons ("30:00" becomes "30😲0").
/// A zero-width space after each colon breaks the pattern without visible change.
fn escape_emoticons(text: &str) -> String {
    text.replace(':', ":\u{200B}")
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

    // Encode the clips up front: validates the files and makes playback a cheap send-per-tick.
    let clip_paths = [&args.warn_60s, &args.warn_30s, &args.warn_15s];
    let mut clips = Vec::with_capacity(clip_paths.len());
    for (&offset, path) in timer::ANNOUNCE_OFFSETS.iter().zip(clip_paths) {
        let frames = audio::encode_clip(path, args.volume)
            .with_context(|| format!("failed to load {offset}s announcement {}", path.display()))?;
        clips.push((offset, frames));
    }

    let group_id = args.whisper_server_group_id;
    let scope = args.whisper_scope;
    let mut target = match group_id {
        Some(group_id) => PlaybackTarget::ServerGroup { group_id, scope },
        None => PlaybackTarget::CurrentChannel,
    };

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

    // The server echoes our own text messages back as events; remember our id to skip them.
    let own_id = con.get_state()?.own_client;

    info!("Connected. can_send_audio = {}", con.can_send_audio());
    match &target {
        PlaybackTarget::ServerGroup { group_id, scope } => {
            info!("Whisper mode: server group {group_id}, scope={scope:?}");
        }
        PlaybackTarget::CurrentChannel => {
            info!("Normal mode: playing into the bot's current channel");
        }
    }

    info!("Jungle timer is stopped. Send '!jungle start' in TeamSpeak chat to begin.");
    info!("Commands: '!jungle start [MM:SS] [at MM:SS]', '!jungle set MM:SS', '!jungle stop'.");

    let mut state = TimerState::Stopped;
    let mut playback: Option<Playback> = None;

    loop {
        // What fires next: the pending audio frame while a clip is streaming,
        // otherwise the next timer transition.
        let pending = if let Some(p) = &playback {
            Some((p.next_at, Due::Frame))
        } else {
            match state {
                TimerState::Stopped => None,
                TimerState::Countdown { starts_at, elapsed } => Some((
                    starts_at,
                    Due::GameStart {
                        game_zero: starts_at.checked_sub(elapsed).unwrap_or(starts_at),
                    },
                )),
                TimerState::Running { game_zero } => match timer::next_announcement(game_zero) {
                    Some((play_at, offset)) => Some((play_at, Due::Announce { offset, game_zero })),
                    None => {
                        info!("Jungle timer finished all announcements.");
                        state = TimerState::Stopped;
                        continue;
                    }
                },
            }
        };

        tokio::select! {
            // into_future() consumes the stream so the future owns the &mut con borrow;
            // map() drops the stream remainder with it, so the branch output carries no
            // borrow and the handler body can use con again.
            item = con.events().into_future().map(|(item, _)| item) => {
                let item = item
                    .transpose()
                    .context("TeamSpeak event stream ended with an error")?;
                let Some(item) = item else {
                    bail!("TeamSpeak connection closed");
                };
                let StreamItem::BookEvents(events) = item else {
                    continue;
                };

                for event in events {
                    let Event::Message { invoker, message, .. } = event else {
                        continue;
                    };
                    if invoker.id == own_id {
                        continue;
                    }
                    let Some(parsed) = timer::parse_timer_command(&message) else {
                        continue;
                    };
                    let reply = match parsed {
                        Ok(command) => {
                            info!(client = %invoker.id, command = %message, "command received");

                            if matches!(command, TimerCommand::Stop) && playback.take().is_some() {
                                // End the aborted audio stream cleanly.
                                con.send_audio(audio::make_packet(&target, &[]))?;
                            }

                            let (new_state, reply) =
                                timer::handle_command(command, &state, &mut target, group_id, scope);
                            state = new_state;
                            reply
                        }
                        Err(err) => {
                            warn!(client = %invoker.id, error = %err, "invalid command");
                            err
                        }
                    };

                    OutSendTextMessagePart {
                        target: TextMessageTargetMode::Client,
                        target_client_id: Some(invoker.id),
                        message: escape_emoticons(&reply).into(),
                    }
                    .send(&mut con)?;
                }
            }
            // The dummy deadline is never polled: the branch is disabled when pending is None.
            _ = time::sleep_until(pending.map_or_else(Instant::now, |(at, _)| at)), if pending.is_some() => {
                match pending.unwrap().1 {
                    Due::Frame => {
                        let p = playback.as_mut().expect("Due::Frame implies active playback");
                        match p.frames.get(p.idx) {
                            Some(frame) => {
                                con.send_audio(audio::make_packet(&target, frame))
                                    .context("failed to send TeamSpeak audio packet")?;
                                p.idx += 1;
                                p.next_at += audio::FRAME_DURATION;
                            }
                            None => {
                                con.send_audio(audio::make_packet(&target, &[]))?;
                                playback = None;
                            }
                        }
                    }
                    Due::GameStart { game_zero } => {
                        info!("Jungle timer started.");
                        state = TimerState::Running { game_zero };
                    }
                    Due::Announce { offset, game_zero } => {
                        let (_, frames) = clips
                            .iter()
                            .find(|(o, _)| *o == offset)
                            .expect("a clip is loaded for every announce offset");
                        info!(
                            offset,
                            remaining = timer::format_remaining(Instant::now().duration_since(game_zero)),
                            "playing announcement",
                        );
                        playback = Some(Playback { frames, idx: 0, next_at: Instant::now() });
                    }
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
