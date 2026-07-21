use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

pub const GAME_LENGTH: Duration = Duration::from_secs(30 * 60);
const JUNGLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const ANNOUNCE_OFFSETS: &[u64] = &[60, 30, 15];
/// Upper bound on user-supplied MM:SS values; keeps `Instant + delay` from overflowing.
const MAX_MINUTES: u64 = 24 * 60;

#[derive(Debug, Clone, Copy)]
pub enum TimerState {
    Stopped,
    Countdown { starts_at: Instant, elapsed: Duration },
    Running { game_zero: Instant },
}

/// A parsed chat command: either for the timer core, or platform-specific.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    Timer(TimerCommand),
    /// Handled by the chat backend: each platform replies with its own help text.
    Help,
    /// TS3-specific: play into the bot's current channel.
    Channel,
    /// TS3-specific: whisper to the configured server group.
    Group,
}

#[derive(Debug, Clone, Copy)]
pub enum TimerCommand {
    Start { elapsed: Duration, delay: Duration },
    Stop,
    Status,
}

pub fn next_announcement(game_zero: Instant) -> Option<(Instant, u64)> {
    let e = Instant::now().duration_since(game_zero).as_secs();
    let mut spawn = (e / JUNGLE_INTERVAL.as_secs() + 1) * JUNGLE_INTERVAL.as_secs();
    while spawn < GAME_LENGTH.as_secs() {
        for &offset in ANNOUNCE_OFFSETS {
            let at = spawn.saturating_sub(offset);
            if at > e {
                return Some((game_zero + Duration::from_secs(at), offset));
            }
        }
        spawn += JUNGLE_INTERVAL.as_secs();
    }
    None
}

/// Applies a timer command, returning the new state and the reply for the sender.
pub fn handle_command(command: TimerCommand, state: &TimerState) -> (TimerState, String) {
    match command {
        TimerCommand::Start { elapsed, delay } => start_timer(elapsed, delay),
        TimerCommand::Stop => {
            let msg = match state {
                TimerState::Stopped => "timer is already stopped",
                _ => "timer stopped",
            };
            info!("{}", msg);
            (TimerState::Stopped, msg.into())
        }
        TimerCommand::Status => {
            let msg = status_message(state);
            info!("{}", msg);
            (*state, msg)
        }
    }
}

fn start_timer(elapsed: Duration, delay: Duration) -> (TimerState, String) {
    if elapsed > GAME_LENGTH {
        warn!("cannot start: game is already over");
        return (
            TimerState::Stopped,
            "cannot start: game is already over".into(),
        );
    }

    let now = Instant::now();

    if delay > Duration::ZERO {
        let msg = format!(
            "countdown started: game begins in {} at {}",
            format_duration(delay),
            format_remaining(elapsed),
        );
        info!("{}", msg);
        return (
            TimerState::Countdown {
                starts_at: now + delay,
                elapsed,
            },
            msg,
        );
    }

    let game_zero = now.checked_sub(elapsed).unwrap_or(now);
    let (state, msg) = match next_announcement(game_zero) {
        None => (
            TimerState::Stopped,
            format!(
                "started at {}: no remaining announcements",
                format_remaining(elapsed),
            ),
        ),
        Some((play_at, offset)) => (
            TimerState::Running { game_zero },
            format!(
                "started at {}; next announcement at {} ({offset}s warning)",
                format_remaining(elapsed),
                format_remaining(play_at.duration_since(game_zero)),
            ),
        ),
    };
    info!("{}", msg);
    (state, msg)
}

pub fn status_message(state: &TimerState) -> String {
    match state {
        TimerState::Stopped => "timer is stopped".into(),
        TimerState::Countdown { starts_at, .. } => {
            format!(
                "countdown: game begins in {}",
                format_duration(starts_at.duration_since(Instant::now()))
            )
        }
        TimerState::Running { game_zero } => {
            let elapsed = Instant::now().duration_since(*game_zero);
            match next_announcement(*game_zero) {
                Some((play_at, offset)) => {
                    format!(
                        "running at {}; next announcement at {} ({offset}s warning)",
                        format_remaining(elapsed),
                        format_remaining(play_at.duration_since(*game_zero)),
                    )
                }
                None => format!(
                    "running at {}: no remaining announcements",
                    format_remaining(elapsed),
                ),
            }
        }
    }
}

pub fn parse_timer_command(message: &str) -> Option<Result<Command, String>> {
    let text = message.trim().strip_prefix('!')?;
    let mut parts = text.split_whitespace();
    let root = parts.next()?;

    if !root.eq_ignore_ascii_case("jungle") {
        return None;
    }

    let action = parts.next().unwrap_or("help").to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();

    let command = match action.as_str() {
        "start" => parse_start(&rest).map(Command::Timer),
        "set" => parse_set(&rest).map(Command::Timer),
        "stop" => Ok(Command::Timer(TimerCommand::Stop)),
        "status" => Ok(Command::Timer(TimerCommand::Status)),
        "help" => Ok(Command::Help),
        "channel" => Ok(Command::Channel),
        "group" => Ok(Command::Group),
        _ => Err(format!("unknown command '{action}'. Try '!jungle help'.")),
    };

    Some(command)
}

fn parse_start(args: &[&str]) -> Result<TimerCommand, String> {
    let (elapsed, delay) = match args {
        [] => (Duration::ZERO, Duration::ZERO),
        [delay] => (Duration::ZERO, parse_mmss(delay)?),
        [at, time] if at.eq_ignore_ascii_case("at") => (parse_gametime(time)?, Duration::ZERO),
        [delay, at, time] if at.eq_ignore_ascii_case("at") => {
            (parse_gametime(time)?, parse_mmss(delay)?)
        }
        _ => return Err("usage: '!jungle start [MM:SS] [at MM:SS]'".into()),
    };
    Ok(TimerCommand::Start { elapsed, delay })
}

fn parse_set(args: &[&str]) -> Result<TimerCommand, String> {
    match args {
        [time] => Ok(TimerCommand::Start {
            elapsed: parse_gametime(time)?,
            delay: Duration::ZERO,
        }),
        _ => Err("expected game time as MM:SS, e.g. '!jungle set 25:00'".into()),
    }
}

/// Converts a game clock reading ("25:00 on the clock") into elapsed game time.
pub fn parse_gametime(text: &str) -> Result<Duration, String> {
    GAME_LENGTH
        .checked_sub(parse_mmss(text)?)
        .ok_or_else(|| "game time cannot exceed 30:00".into())
}

pub fn parse_mmss(text: &str) -> Result<Duration, String> {
    let invalid = || format!("'{text}' is not a valid time; expected MM:SS");
    let (min, sec) = text.split_once(':').ok_or_else(invalid)?;
    let m: u64 = min.parse().map_err(|_| invalid())?;
    let s: u64 = sec.parse().map_err(|_| invalid())?;
    if s >= 60 {
        return Err(invalid());
    }
    if m > MAX_MINUTES {
        return Err(format!("'{text}': number too large (max {MAX_MINUTES} minutes)"));
    }
    Ok(Duration::from_secs(m * 60 + s))
}

pub fn format_remaining(elapsed: Duration) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_start_variants() {
        // (message, expected elapsed seconds, expected delay seconds)
        let cases = [
            ("!jungle start", 0, 0),
            ("!jungle start 3:00", 0, 180),
            ("!jungle start at 25:00", 300, 0),
            ("!jungle start 3:00 AT 25:00", 300, 180),
            ("!jungle set 25:00", 300, 0),
        ];
        for (msg, elapsed, delay) in cases {
            match parse_timer_command(msg).unwrap().unwrap() {
                Command::Timer(TimerCommand::Start { elapsed: e, delay: d }) => {
                    assert_eq!(e.as_secs(), elapsed, "{msg}");
                    assert_eq!(d.as_secs(), delay, "{msg}");
                }
                other => panic!("{msg} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_oversized_numbers() {
        // Values this large would overflow `Instant + delay` and crash the bot.
        for msg in [
            "!jungle start 99999999999999999:00",
            "!jungle start at 99999999999999999:00",
            "!jungle set 99999999999999999:00",
        ] {
            let err = parse_timer_command(msg).unwrap().unwrap_err();
            assert!(err.contains("too large"), "{msg}: {err}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for msg in [
            "!jungle start 3:99",
            "!jungle start banana",
            "!jungle start 3:00 at 25:00 extra",
            "!jungle set 31:00",
            "!jungle set",
        ] {
            assert!(parse_timer_command(msg).unwrap().is_err(), "{msg}");
        }
        assert!(parse_timer_command("hello").is_none());
        assert!(parse_timer_command("!other start").is_none());
    }
}
