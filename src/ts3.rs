use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Channels, SampleRate};
use clap::ValueEnum;
use futures::{future, prelude::*};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tracing::{info, warn};
use ts_bookkeeping::messages::c2s::OutSendTextMessagePart;
use ts_bookkeeping::TextMessageTargetMode;
use tsclientlib::{
    events::Event, ChannelId, Connection, DisconnectOptions, Identity, OutCommandExt, StreamItem,
};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

use crate::audio;
use crate::timer::{self, Command, Sound, TimerCommand, TimerState};
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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// A command sent by the GUI thread to the running bot.
#[derive(Debug, Clone, Copy)]
pub enum GuiCommand {
    Command(Command),
    Disconnect,
}

/// An event pushed from the bot to the GUI thread.
#[derive(Debug, Clone)]
pub enum BotEvent {
    /// The connection is up and commands are accepted.
    Connected,
    /// The timer state changed (also sent right after connecting).
    Timer(TimerState),
    /// Whether audio currently goes to the server group (true) or channel.
    Whispering(bool),
    /// The bot loop ended; carries the error message if it failed.
    Stopped(Option<String>),
}

pub type EventSink = Box<dyn Fn(BotEvent) + Send>;

/// Channel pair connecting a GUI frontend to the bot loop.
pub struct GuiBridge {
    pub commands: mpsc::UnboundedReceiver<GuiCommand>,
    pub events: EventSink,
}

fn emit(sink: &Option<EventSink>, event: BotEvent) {
    if let Some(sink) = sink {
        sink(event);
    }
}

/// An announcement clip mid-stream: precomputed Opus frames, sent one per tick.
struct Playback<'a> {
    frames: &'a [Vec<u8>],
    idx: usize,
    next_at: Instant,
    /// Where this clip goes; zeal can whisper to its own server group.
    target: PlaybackTarget,
}

/// What the next deadline does when it fires.
#[derive(Clone, Copy)]
enum Due {
    Frame,
    GameStart { game_zero: Instant },
    Announce { play_at: Instant, game_zero: Instant },
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

/// Applies a bot command to the running state, returning the reply text.
/// Shared by the chat-message path and the GUI path.
fn apply_command<'a>(
    command: Command,
    con: &mut Connection,
    state: &mut TimerState,
    target: &mut PlaybackTarget,
    playback: &mut Option<Playback<'a>>,
    queue: &mut Vec<Sound>,
    whisper: Option<(u64, WhisperScope)>,
) -> Result<String> {
    let reply = match command {
        Command::Help => HELP.into(),
        Command::Channel => {
            info!("switched to channel playback");
            *target = PlaybackTarget::CurrentChannel;
            "switched to channel playback".into()
        }
        Command::Group => match whisper {
            Some((id, scope)) => {
                info!(group = id, "switched to server group whisper");
                *target = PlaybackTarget::ServerGroup { group_id: id, scope };
                format!("switched to server group whisper (group {id})")
            }
            None => {
                warn!("no server group configured");
                "no server group configured (use --whisper-server-group-id at startup)".into()
            }
        },
        Command::Timer(command) => {
            if matches!(command, TimerCommand::Stop) {
                queue.clear();
                if let Some(p) = playback.take() {
                    // End the aborted audio stream cleanly.
                    con.send_audio(make_packet(&p.target, &[]))?;
                }
            }

            let (new_state, reply) = timer::handle_command(command, state);
            *state = new_state;
            reply
        }
    };
    Ok(reply)
}

/// Connects to the TeamSpeak server and runs the bot until the connection
/// closes, Ctrl-C, or a GUI disconnect. `pcm_clips` pairs each sound with its
/// clip. `gui` is None when running headless from the CLI.
pub async fn run(
    args: &Args,
    identity: Option<Identity>,
    pcm_clips: &[(Sound, Vec<f32>)],
    gui: Option<GuiBridge>,
) -> Result<()> {
    // Encode the clips up front: validates them and makes playback a cheap send-per-tick.
    let mut clips = Vec::with_capacity(pcm_clips.len());
    for (sound, pcm) in pcm_clips {
        let frames = encode_frames(pcm)
            .with_context(|| format!("failed to encode {sound:?} clip"))?;
        clips.push((*sound, frames));
    }

    let group_id = args.whisper_server_group_id;
    let scope = args.whisper_scope;
    let whisper = group_id.map(|id| (id, scope));
    // Zeal whispers to its own group when configured, else to the jungle target.
    let zeal_group = args.zeal_server_group_id.map(|id| (id, scope));
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
    if let Some(identity) = identity {
        opts = opts.identity(identity);
    }

    let (mut gui_rx, ui_events) = match gui {
        Some(bridge) => (Some(bridge.commands), Some(bridge.events)),
        None => (None, None),
    };

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
    if let Some((group_id, scope)) = zeal_group {
        info!("Zeal mode: whispers to server group {group_id}, scope={scope:?}");
    }

    info!("Jungle timer is stopped. Send '!jungle start' in TeamSpeak chat to begin.");
    info!("Commands: '!jungle start [MM:SS] [at MM:SS]', '!jungle set MM:SS', '!jungle stop'.");

    let mut state = TimerState::Stopped;
    let mut playback: Option<Playback> = None;
    // Sounds whose turn came while a clip was streaming; played back to back,
    // jungle warnings before zeal.
    let mut queue: Vec<Sound> = Vec::new();

    emit(&ui_events, BotEvent::Connected);
    emit(&ui_events, BotEvent::Timer(state));
    emit(
        &ui_events,
        BotEvent::Whispering(matches!(target, PlaybackTarget::ServerGroup { .. })),
    );

    loop {
        // The line is free: start the next queued sound, if any.
        if playback.is_none() && !queue.is_empty() {
            let jungle = queue
                .iter()
                .position(|s| matches!(s, Sound::Jungle(_)))
                .unwrap_or(0);
            let sound = queue.remove(jungle);
            let (_, frames) = clips
                .iter()
                .find(|(s, _)| *s == sound)
                .expect("a clip is loaded for every sound");
            info!(?sound, "playing sound");
            let play_target = match (sound, zeal_group) {
                (Sound::Zeal, Some((group_id, scope))) => {
                    PlaybackTarget::ServerGroup { group_id, scope }
                }
                _ => target.clone(),
            };
            playback = Some(Playback {
                frames,
                idx: 0,
                next_at: Instant::now(),
                target: play_target,
            });
        }

        // What fires next: the pending audio frame while a clip is streaming,
        // or the next timer transition — whichever comes first.
        let frame = playback.as_ref().map(|p| (p.next_at, Due::Frame));
        let transition = match state {
            TimerState::Stopped => None,
            TimerState::Countdown { starts_at, elapsed } => Some((
                starts_at,
                Due::GameStart {
                    game_zero: starts_at.checked_sub(elapsed).unwrap_or(starts_at),
                },
            )),
            TimerState::Running { game_zero } => match timer::next_announcement(game_zero) {
                Some((play_at, _)) => Some((play_at, Due::Announce { play_at, game_zero })),
                None => {
                    info!("Jungle timer finished all announcements.");
                    state = TimerState::Stopped;
                    emit(&ui_events, BotEvent::Timer(state));
                    continue;
                }
            },
        };
        let pending = [frame, transition]
            .into_iter()
            .flatten()
            .min_by_key(|(at, _)| *at);

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
                        Ok(command) => {
                            let reply = apply_command(
                                command,
                                &mut con,
                                &mut state,
                                &mut target,
                                &mut playback,
                                &mut queue,
                                whisper,
                            )?;
                            emit(&ui_events, BotEvent::Timer(state));
                            emit(
                                &ui_events,
                                BotEvent::Whispering(matches!(
                                    target,
                                    PlaybackTarget::ServerGroup { .. }
                                )),
                            );
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
                                con.send_audio(make_packet(&p.target, frame))
                                    .context("failed to send TeamSpeak audio packet")?;
                                p.idx += 1;
                                p.next_at += FRAME_DURATION;
                            }
                            None => {
                                let packet = make_packet(&p.target, &[]);
                                con.send_audio(packet)?;
                                playback = None;
                            }
                        }
                    }
                    Due::GameStart { game_zero } => {
                        info!("Jungle timer started.");
                        state = TimerState::Running { game_zero };
                        emit(&ui_events, BotEvent::Timer(state));
                    }
                    Due::Announce { play_at, game_zero } => {
                        // Everything due this second goes on the queue (jungle
                        // first); the pump at the top of the loop starts it
                        // now or after the current clip ends.
                        let at = play_at.duration_since(game_zero).as_secs();
                        queue.extend(timer::due_sounds(at));
                    }
                }
            }
            // GUI buttons produce the same commands as chat messages; the
            // branch is disabled in CLI mode.
            command = async { gui_rx.as_mut().expect("branch disabled without gui").recv().await }, if gui_rx.is_some() => {
                match command {
                    Some(GuiCommand::Command(command)) => {
                        apply_command(
                            command,
                            &mut con,
                            &mut state,
                            &mut target,
                            &mut playback,
                            &mut queue,
                            whisper,
                        )?;
                        emit(&ui_events, BotEvent::Timer(state));
                        emit(
                            &ui_events,
                            BotEvent::Whispering(matches!(target, PlaybackTarget::ServerGroup { .. })),
                        );
                    }
                    // None: the GUI dropped its sender, treat it as disconnect.
                    Some(GuiCommand::Disconnect) | None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    info!("Disconnecting ...");
    con.disconnect(DisconnectOptions::new())?;
    // The disconnect packet is only sent (and acked) while the connection
    // keeps being polled; breaking straight out would leave the server to
    // time the client out instead.
    let drain = con.events().for_each(|_| future::ready(()));
    if time::timeout(Duration::from_secs(3), drain).await.is_err() {
        warn!("server did not acknowledge the disconnect in time");
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
