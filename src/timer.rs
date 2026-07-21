use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

pub const GAME_LENGTH: Duration = Duration::from_secs(30 * 60);
const JUNGLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const ANNOUNCE_OFFSETS: &[u64] = &[60, 40, 20];
/// Zeal spawns 15 s after game start (29:45 on the clock), then every
/// 3:05 — the last one lands at 08:10, the next (05:05) is suppressed.
const ZEAL_FIRST: Duration = Duration::from_secs(15);
const ZEAL_INTERVAL: Duration = Duration::from_secs(3 * 60 + 5);
const ZEAL_LAST: Duration = Duration::from_secs(22 * 60); // elapsed time at clock 08:00
/// Upper bound on user-supplied MM:SS values; keeps `Instant + delay` from overflowing.
const MAX_MINUTES: u64 = 24 * 60;

/// A scheduled sound: a jungle warning `offset` seconds before a camp spawn,
/// or the zeal spawn itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Jungle(u64),
    Zeal,
}

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

/// All sounds scheduled exactly `at` seconds after game start, jungle
/// warnings first. Single source of truth for the schedule; almost always
/// one entry, but several when timings are changed to overlap — the bot
/// queues those and plays them back to back.
pub fn due_sounds(at: u64) -> Vec<Sound> {
    let mut sounds = Vec::new();
    let interval = JUNGLE_INTERVAL.as_secs();
    for &offset in ANNOUNCE_OFFSETS {
        let spawn = at + offset;
        if spawn >= interval && spawn < GAME_LENGTH.as_secs() && spawn.is_multiple_of(interval) {
            sounds.push(Sound::Jungle(offset));
        }
    }
    if (ZEAL_FIRST.as_secs()..=ZEAL_LAST.as_secs()).contains(&at)
        && (at - ZEAL_FIRST.as_secs()).is_multiple_of(ZEAL_INTERVAL.as_secs())
    {
        sounds.push(Sound::Zeal);
    }
    sounds
}

/// The next sound to play, whichever comes first. Overlapping events on the
/// same second are reported one per call (jungle first); the caller drains
/// the rest via [`due_sounds`].
pub fn next_announcement(game_zero: Instant) -> Option<(Instant, Sound)> {
    let e = Instant::now().duration_since(game_zero).as_secs();
    next_after(e).map(|(at, sound)| (game_zero + Duration::from_secs(at), sound))
}

/// The first scheduled sound strictly after `e` seconds of game time.
fn next_after(e: u64) -> Option<(u64, Sound)> {
    (e + 1..GAME_LENGTH.as_secs()).find_map(|at| due_sounds(at).first().map(|s| (at, *s)))
}

/// "...; next announcement at 24:00 (60s warning)" / "...; next zeal at 29:45".
fn next_message(prefix: &str, play_at: Instant, game_zero: Instant, sound: Sound) -> String {
    let at = format_remaining(play_at.duration_since(game_zero));
    match sound {
        Sound::Jungle(offset) => format!("{prefix}; next announcement at {at} ({offset}s warning)"),
        Sound::Zeal => format!("{prefix}; next zeal at {at}"),
    }
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
        Some((play_at, sound)) => (
            TimerState::Running { game_zero },
            next_message(
                &format!("started at {}", format_remaining(elapsed)),
                play_at,
                game_zero,
                sound,
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
                Some((play_at, sound)) => next_message(
                    &format!("running at {}", format_remaining(elapsed)),
                    play_at,
                    *game_zero,
                    sound,
                ),
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
    fn schedules_zeal_spawns() {
        let zeals: Vec<u64> = (0..GAME_LENGTH.as_secs())
            .filter(|&at| due_sounds(at).contains(&Sound::Zeal))
            .collect();
        // Every 3:05 from 0:15 elapsed (29:45 on the clock): 29:45, 26:40,
        // 23:35, 20:30, 17:25, 14:20, 11:15, 08:10 — 05:05 is suppressed.
        assert_eq!(zeals, vec![15, 200, 385, 570, 755, 940, 1125, 1310]);
    }

    #[test]
    fn schedules_jungle_warnings() {
        for (offset, first) in [(60, 240), (40, 260), (20, 280)] {
            let warnings: Vec<u64> = (0..GAME_LENGTH.as_secs())
                .filter(|&at| due_sounds(at).contains(&Sound::Jungle(offset)))
                .collect();
            // Each offset fires 5 times, once per spawn mark; none before
            // the 30:00 start or after the 0:00 end.
            assert_eq!(
                warnings,
                (0..5).map(|i| first + i * 300).collect::<Vec<_>>(),
                "{offset}s warning"
            );
        }
    }

    #[test]
    fn zeal_and_jungle_currently_never_collide() {
        // Overlaps are handled (queued, jungle first), so a failure here is
        // not a bug — it just means the timings changed and playback order
        // for the overlap should be double-checked.
        for at in 0..GAME_LENGTH.as_secs() {
            let sounds = due_sounds(at);
            let jungle = sounds.iter().any(|s| matches!(s, Sound::Jungle(_)));
            let zeal = sounds.contains(&Sound::Zeal);
            assert!(!(jungle && zeal), "collision at {at}");
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
