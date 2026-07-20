use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Channels, SampleRate};
use clap::ValueEnum;
use futures::{future, prelude::*};
use tokio::time::{self, Instant};
use tracing::{info, warn};
use ts_bookkeeping::messages::c2s::OutSendTextMessagePart;
use ts_bookkeeping::TextMessageTargetMode;
use tsclientlib::{
    events::Event, ChannelId, Connection, DisconnectOptions, Identity, OutCommandExt, StreamItem,
};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

use crate::audio;
use crate::timer::{self, Command, TimerCommand, TimerState};
use crate::Args;

const SAMPLES_PER_FRAME: usize = audio::SAMPLE_RATE / 50;
const MAX_OPUS_FRAME_SIZE: usize = 1_275;
const FRAME_DURATION: Duration = Duration::from_millis(20);

// Starts with a newline so the first line aligns with the rest in chat,
// instead of hanging behind the "<time> \"BotName\":" prefix.
const HELP: &str = "
!jungle start                     start at 30:00 now
!jungle start 3:00                start at 30:00 in 3 minutes
!jungle start at 25:00            start immediately at 25:00
!jungle start 3:00 at 25:00       start at 25:00 in 3 minutes
!jungle set 25:00                 set game time to 25:00
!jungle channel                   play to current channel
!jungle group                     whisper to configured server group
!jungle stop                      stop the timer
!jungle status                    print current timer state
!jungle help                      show this message";

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhisperScope {
    AllChannels,
    CurrentChannel,
}

impl WhisperScope {
    fn wire_value(self) -> u8 {
        match self {
            WhisperScope::AllChannels => 0,
            WhisperScope::CurrentChannel => 1,
        }
    }
}

#[derive(Debug, Clone)]
enum PlaybackTarget {
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

/// Opus-encodes 48 kHz mono PCM into ready-to-send 20 ms frames.
fn encode_frames(pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
    let mut samples = pcm.to_vec();
    samples.resize(samples.len().next_multiple_of(SAMPLES_PER_FRAME), 0.0);

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        .map_err(|e| anyhow!("failed to create Opus encoder: {e:?}"))?;
    let mut opus_frame = [0_u8; MAX_OPUS_FRAME_SIZE];
    let mut frames = Vec::with_capacity(samples.len() / SAMPLES_PER_FRAME);

    for chunk in samples.chunks(SAMPLES_PER_FRAME) {
        let len = encoder
            .encode_float(chunk, &mut opus_frame)
            .map_err(|e| anyhow!("failed to encode Opus frame: {e:?}"))?;
        frames.push(opus_frame[..len].to_vec());
    }

    Ok(frames)
}

fn make_packet(target: &PlaybackTarget, data: &[u8]) -> OutPacket {
    let codec = CodecType::OpusVoice;
    match target {
        PlaybackTarget::CurrentChannel => OutAudio::new(&AudioData::C2S { id: 0, codec, data }),
        PlaybackTarget::ServerGroup { group_id, scope } => {
            OutAudio::new(&AudioData::C2SWhisperNew {
                id: 0,
                codec,
                whisper_type: 0,
                target: scope.wire_value(),
                target_id: *group_id,
                data,
            })
        }
    }
}

/// Connects to the TeamSpeak server and runs the bot until the connection
/// closes or Ctrl-C. `pcm_clips` pairs each announce offset with its clip.
pub async fn run(args: &Args, pcm_clips: &[(u64, Vec<f32>)]) -> Result<()> {
    // Encode the clips up front: validates them and makes playback a cheap send-per-tick.
    let mut clips = Vec::with_capacity(pcm_clips.len());
    for (offset, pcm) in pcm_clips {
        let frames = encode_frames(pcm)
            .with_context(|| format!("failed to encode {offset}s announcement"))?;
        clips.push((*offset, frames));
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
                    if parsed.is_ok() {
                        info!(client = %invoker.id, command = %message, "command received");
                    }

                    let reply = match parsed {
                        Ok(Command::Help) => HELP.into(),
                        Ok(Command::Channel) => {
                            info!("switched to channel playback");
                            target = PlaybackTarget::CurrentChannel;
                            "switched to channel playback".into()
                        }
                        Ok(Command::Group) => match group_id {
                            Some(id) => {
                                info!(group = id, "switched to server group whisper");
                                target = PlaybackTarget::ServerGroup { group_id: id, scope };
                                format!("switched to server group whisper (group {id})")
                            }
                            None => {
                                warn!("no server group configured");
                                "no server group configured (use --whisper-server-group-id at startup)"
                                    .into()
                            }
                        },
                        Ok(Command::Timer(command)) => {
                            if matches!(command, TimerCommand::Stop) && playback.take().is_some() {
                                // End the aborted audio stream cleanly.
                                con.send_audio(make_packet(&target, &[]))?;
                            }

                            let (new_state, reply) = timer::handle_command(command, &state);
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
                                con.send_audio(make_packet(&target, frame))
                                    .context("failed to send TeamSpeak audio packet")?;
                                p.idx += 1;
                                p.next_at += FRAME_DURATION;
                            }
                            None => {
                                con.send_audio(make_packet(&target, &[]))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pcm_into_20ms_frames() {
        // Half a second of silence at 48 kHz = exactly 25 frames of 960 samples.
        let frames = encode_frames(&vec![0.0; 24_000]).unwrap();
        assert_eq!(frames.len(), 25);
        assert!(frames.iter().all(|f| !f.is_empty()));
    }
}
