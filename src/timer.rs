use std::{path::Path, time::Duration};

use anyhow::Result;
use tokio::{
    sync::mpsc,
    time::{self, Instant},
};
use tracing::{info, warn};
use tsproto_packets::packets::OutPacket;

use crate::audio;
use crate::{Inbound, Args, PlaybackTarget, WhisperScope};
use ts_bookkeeping::ClientId;

const GAME_LENGTH: Duration = Duration::from_secs(30 * 60);
const JUNGLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const ANNOUNCE_OFFSETS: &[(u64, &str)] = &[(60, "60s"), (30, "30s"), (15, "15s")];

#[derive(Debug, Clone, Copy)]
pub enum TimerState {
    Stopped,
    Countdown { starts_at: Instant, elapsed: Duration },
    Running { game_zero: Instant },
}

#[derive(Debug, Clone, Copy)]
pub enum TimerCommand {
    Start { elapsed: Duration, delay: Duration },
    Stop,
    Status,
    Help,
    Channel,
    Group,
}

fn next_announcement(game_zero: Instant) -> Option<(Instant, &'static str)> {
    let elapsed = Instant::now().duration_since(game_zero);
    let e = elapsed.as_secs();
    let mut spawn = (e / JUNGLE_INTERVAL.as_secs() + 1) * JUNGLE_INTERVAL.as_secs();
    while spawn < GAME_LENGTH.as_secs() {
        for &(offset, label) in ANNOUNCE_OFFSETS {
            let at = spawn.saturating_sub(offset);
            if at > e {
                return Some((game_zero + Duration::from_secs(at), label));
            }
        }
        spawn += JUNGLE_INTERVAL.as_secs();
    }
    None
}

pub async fn jungle_timer(
    args: Args,
    mut target: PlaybackTarget,
    tx: mpsc::Sender<OutPacket>,
    mut commands: mpsc::Receiver<Inbound>,
    response_tx: mpsc::Sender<(ClientId, String)>,
) -> Result<()> {
    let group_id = args.whisper_server_group_id;
    let scope = args.whisper_scope;
    let mut state = TimerState::Stopped;

    loop {
        state = match state {
            TimerState::Stopped => {
                let Some(inbound) = commands.recv().await else {
                    return Ok(());
                };
                let (new_state, tc) = handle_command(
                    inbound.command,
                    &TimerState::Stopped,
                    group_id,
                    scope,
                    &response_tx,
                    inbound.from_client,
                );
                if let Some(t) = tc {
                    target = t;
                }
                new_state
            }
            TimerState::Countdown {
                starts_at,
                elapsed,
            } => {
                tokio::select! {
                    command = commands.recv() => {
                        let Some(inbound) = command else { return Ok(()); };
                        let current = TimerState::Countdown { starts_at, elapsed };
                        let (new_state, tc) = handle_command(
                            inbound.command, &current, group_id, scope,
                            &response_tx, inbound.from_client,
                        );
                        if let Some(t) = tc { target = t; }
                        new_state
                    }
                    _ = time::sleep_until(starts_at) => {
                        let game_zero = starts_at.checked_sub(elapsed).unwrap_or(starts_at);
                        info!("Jungle timer started.");
                        TimerState::Running { game_zero }
                    }
                }
            }
            TimerState::Running { game_zero } => match next_announcement(game_zero) {
                None => {
                    info!("Jungle timer finished all announcements.");
                    TimerState::Stopped
                }
                Some((play_at, label)) => {
                    let file: &Path = match label {
                        "60s" => &args.warn_60s,
                        "30s" => &args.warn_30s,
                        _ => &args.warn_15s,
                    };
                    tokio::select! {
                        command = commands.recv() => {
                            let Some(inbound) = command else { return Ok(()); };
                            let current = TimerState::Running { game_zero };
                            let (new_state, tc) = handle_command(
                                inbound.command, &current, group_id, scope,
                                &response_tx, inbound.from_client,
                            );
                            if let Some(t) = tc { target = t; }
                            new_state
                        }
                        _ = time::sleep_until(play_at) => {
                            info!(
                                label,
                                remaining = format_remaining(Instant::now().duration_since(game_zero)),
                                file = %file.display(),
                                "playing announcement",
                            );
                            audio::play_audio_once(file, args.volume, &target, &tx).await?;
                            TimerState::Running { game_zero }
                        }
                    }
                }
            },
        };
    }
}

fn respond(
    tx: &mpsc::Sender<(ClientId, String)>,
    client: ClientId,
    text: impl Into<String>,
) {
    let _ = tx.try_send((client, text.into()));
}

fn handle_command(
    command: TimerCommand,
    state: &TimerState,
    group_id: Option<u64>,
    scope: WhisperScope,
    response_tx: &mpsc::Sender<(ClientId, String)>,
    from_client: ClientId,
) -> (TimerState, Option<PlaybackTarget>) {
    match command {
        TimerCommand::Channel => {
            info!("switched to channel playback");
            respond(response_tx, from_client, "switched to channel playback");
            (*state, Some(PlaybackTarget::CurrentChannel))
        }
        TimerCommand::Group => match group_id {
            Some(id) => {
                info!(group = id, "switched to server group whisper");
                respond(
                    response_tx,
                    from_client,
                    format!("switched to server group whisper (group {id})"),
                );
                (
                    *state,
                    Some(PlaybackTarget::ServerGroup {
                        group_id: id,
                        scope,
                    }),
                )
            }
            None => {
                warn!("no server group configured");
                respond(
                    response_tx,
                    from_client,
                    "no server group configured (use --whisper-server-group-id at startup)",
                );
                (*state, None)
            }
        },
        other => (apply_timer_command(other, state, response_tx, from_client), None),
    }
}

fn apply_timer_command(
    command: TimerCommand,
    state: &TimerState,
    response_tx: &mpsc::Sender<(ClientId, String)>,
    from_client: ClientId,
) -> TimerState {
    match command {
        TimerCommand::Start { elapsed, delay } => {
            start_timer(elapsed, delay, response_tx, from_client)
        }
        TimerCommand::Stop => {
            match state {
                TimerState::Stopped => {
                    info!("Jungle timer is already stopped.");
                    respond(response_tx, from_client, "timer is already stopped");
                }
                _ => {
                    info!("Jungle timer stopped.");
                    respond(response_tx, from_client, "timer stopped");
                }
            }
            TimerState::Stopped
        }
        TimerCommand::Status => {
            print_timer_status(state, response_tx, from_client);
            *state
        }
        TimerCommand::Help => {
            print_timer_help(response_tx, from_client);
            *state
        }
        TimerCommand::Channel | TimerCommand::Group => unreachable!(),
    }
}

fn start_timer(
    elapsed: Duration,
    delay: Duration,
    response_tx: &mpsc::Sender<(ClientId, String)>,
    from_client: ClientId,
) -> TimerState {
    if elapsed > GAME_LENGTH {
        warn!("cannot start: game is already over");
        respond(response_tx, from_client, "cannot start: game is already over");
        return TimerState::Stopped;
    }

    let now = Instant::now();

    if delay > Duration::ZERO {
        let starts_at = now + delay;
        let msg = format!(
            "countdown started: game begins in {} at {}",
            format_duration(delay),
            format_remaining(elapsed),
        );
        info!("{}", msg);
        respond(response_tx, from_client, msg);
        return TimerState::Countdown {
            starts_at,
            elapsed,
        };
    }

    let game_zero = now.checked_sub(elapsed).unwrap_or(now);
    match next_announcement(game_zero) {
        None => {
            let msg = format!(
                "started at {}: no remaining announcements",
                format_remaining(elapsed),
            );
            info!("{}", msg);
            respond(response_tx, from_client, msg);
            TimerState::Stopped
        }
        Some((_, label)) => {
            let msg = format!(
                "started at {}; next {} announcement",
                format_remaining(elapsed),
                label,
            );
            info!("{}", msg);
            respond(response_tx, from_client, msg);
            TimerState::Running { game_zero }
        }
    }
}

fn print_timer_status(
    state: &TimerState,
    response_tx: &mpsc::Sender<(ClientId, String)>,
    from_client: ClientId,
) {
    let msg = match state {
        TimerState::Stopped => "timer is stopped".to_string(),
        TimerState::Countdown { starts_at, .. } => {
            format!(
                "countdown: game begins in {}",
                format_duration(starts_at.duration_since(Instant::now()))
            )
        }
        TimerState::Running { game_zero } => {
            let elapsed = Instant::now().duration_since(*game_zero);
            match next_announcement(*game_zero) {
                Some((_, label)) => {
                    format!(
                        "running at {}; next {} announcement",
                        format_remaining(elapsed),
                        label,
                    )
                }
                None => format!(
                    "running at {}: no remaining announcements",
                    format_remaining(elapsed),
                ),
            }
        }
    };
    info!("{}", msg);
    respond(response_tx, from_client, msg);
}

fn print_timer_help(
    response_tx: &mpsc::Sender<(ClientId, String)>,
    from_client: ClientId,
) {
    let msg = [
        "!jungle start                     start at 30:00 now",
        "!jungle start 3:00                start at 30:00 in 3 minutes",
        "!jungle start at 25:00            start immediately at 25:00",
        "!jungle start 3:00 at 25:00       start at 25:00 in 3 minutes",
        "!jungle set 25:00                 set game time to 25:00",
        "!jungle channel                   play to current channel",
        "!jungle group                     whisper to configured server group",
        "!jungle stop                      stop the timer",
        "!jungle status                    print current timer state",
        "!jungle help                      show this message",
    ]
    .join("\n");
    respond(response_tx, from_client, msg);
}

pub fn parse_timer_command(message: &str) -> Option<std::result::Result<TimerCommand, String>> {
    let text = message.trim().strip_prefix('!')?;
    let mut parts = text.split_whitespace();
    let root = parts.next()?;

    if !root.eq_ignore_ascii_case("jungle") {
        return None;
    }

    let action = parts.next().unwrap_or("help").to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();

    let command = match action.as_str() {
        "start" => parse_start(&rest),
        "set" => parse_set(&rest),
        "stop" => Ok(TimerCommand::Stop),
        "status" => Ok(TimerCommand::Status),
        "help" => Ok(TimerCommand::Help),
        "channel" => Ok(TimerCommand::Channel),
        "group" => Ok(TimerCommand::Group),
        _ => Err(format!("unknown command '{action}'. Try '!jungle help'.")),
    };

    Some(command)
}

fn parse_start(args: &[&str]) -> std::result::Result<TimerCommand, String> {
    let joined = args.join(" ");

    if joined.is_empty() {
        return Ok(TimerCommand::Start {
            elapsed: Duration::ZERO,
            delay: Duration::ZERO,
        });
    }

    if let Some(at_idx) = joined.find(" at ") {
        let countdown_str = joined[..at_idx].trim();
        let gametime_str = joined[at_idx + 4..].trim();

        let gametime = parse_mmss(gametime_str)
            .ok_or_else(|| "expected game time after 'at', e.g. '3:00 at 25:00'".to_string())?;
        let elapsed = GAME_LENGTH
            .checked_sub(gametime)
            .ok_or_else(|| "game time cannot exceed 30:00".to_string())?;

        let delay = if countdown_str.is_empty() {
            Duration::ZERO
        } else {
            parse_mmss(countdown_str).ok_or_else(|| {
                "expected countdown before 'at', e.g. '3:00 at 25:00'".to_string()
            })?
        };

        Ok(TimerCommand::Start { elapsed, delay })
    } else {
        let delay = parse_mmss(&joined)
            .ok_or_else(|| "expected time as MM:SS, e.g. '!jungle start 3:00'".to_string())?;
        Ok(TimerCommand::Start {
            elapsed: Duration::ZERO,
            delay,
        })
    }
}

fn parse_set(args: &[&str]) -> std::result::Result<TimerCommand, String> {
    let joined = args.join(" ");
    let gametime = parse_mmss(joined.trim())
        .ok_or_else(|| "expected game time as MM:SS, e.g. '!jungle set 25:00'".to_string())?;
    let elapsed = GAME_LENGTH
        .checked_sub(gametime)
        .ok_or_else(|| "game time cannot exceed 30:00".to_string())?;
    Ok(TimerCommand::Start {
        elapsed,
        delay: Duration::ZERO,
    })
}

fn parse_mmss(text: &str) -> Option<Duration> {
    let (min_str, sec_str) = text.split_once(':')?;
    let m: u64 = min_str.parse().ok()?;
    let s: u64 = sec_str.parse().ok()?;
    if s >= 60 {
        return None;
    }
    Some(Duration::from_secs(m * 60 + s))
}

fn format_remaining(elapsed: Duration) -> String {
    let remaining = GAME_LENGTH.saturating_sub(elapsed).as_secs();
    format!("{}:{:02}", remaining / 60, remaining % 60)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
