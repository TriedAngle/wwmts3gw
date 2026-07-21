# wwmts3gw

TeamSpeak 3 jungle timer bot for Where Winds Meet guild wars. Plays audio announcements 60, 40, and 20 seconds before each jungle camp spawn. The game timer counts down from 30:00 with spawns every 5 minutes.

## Dependencies

**Rust** via [rustup](https://rustup.rs) and a **C compiler** (build-essential / Xcode CLI / MSVC Build Tools).
No ffmpeg, no system audio libraries required — everything is compiled in.

| Platform | Install |
|---|---|
| macOS | `brew install rust` |
| Ubuntu/Debian | `sudo apt install -y cargo build-essential libssl-dev libopus-dev pkg-config autoconf automake libtool libopus-dev` |
| Windows | [rustup.rs](https://rustup.rs) + [Build Tools for Visual Studio](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) |

## Build

```bash
cargo build --release
# Binary at target/release/wwmts3gw (or .exe on Windows)
```

## Audio files

Place the announcements in the `assets/` directory (or anywhere, use the flags below):

| File | When it plays |
|---|---|
| `assets/Jungle 60 sec.wav` | 60 seconds before spawn |
| `assets/Jungle 40 sec.wav` | 40 seconds before spawn |
| `assets/Jungle 20 sec.wav` | 20 seconds before spawn |
| `assets/Zeal.wav` | at each zeal spawn |

Any common audio format works (WAV, MP3, FLAC, OGG, ...) at any sample rate; clips are downmixed to mono and resampled to 48 kHz at startup. Use the `--warn-*` flags to override paths:

```bash
--warn-60s /path/to/60s.wav
--warn-40s /path/to/40s.wav
--warn-20s /path/to/20s.wav
--zeal /path/to/Zeal.wav
```

## Run

**Channel playback** (bot speaks to its current channel):

```bash
./target/release/wwmts3gw \
  --server "ts.yourserver.com:9987" \
  --channel "Default Channel" \
  --name "JungleBot"
```

**Whisper to a server group:**

```bash
./target/release/wwmts3gw \
  --server "ts.yourserver.com:9987" \
  --channel "Default Channel" \
  --name "JungleBot" \
  --whisper-server-group-id 12
```

`--whisper-scope` defaults to `all-channels`. Use `current-channel` to whisper only to group members in the bot's current channel.

Stop with `Ctrl+C`.

## TeamSpeak commands

Send as a channel, server, private, or poke message to the bot. All times are `MM:SS`.

| Command | Effect |
|---|---|
| `!jungle start` | Start at **30:00** now |
| `!jungle start 3:00` | 3-minute countdown, then start at **30:00** |
| `!jungle start at 25:00` | Start immediately at **25:00** (late join) |
| `!jungle start 3:00 at 25:00` | 3-minute countdown, then start at **25:00** |
| `!jungle set 25:00` | Set running timer to **25:00** |
| `!jungle channel` | Switch to channel playback |
| `!jungle group` | Switch to server group whisper |
| `!jungle stop` | Stop the timer |
| `!jungle status` | Show current game time and next announcement |
| `!jungle help` | Show all commands |

**Calling `!jungle start` while already running resets the timer.** `!jungle set` adjusts it without restarting.

## Spawn schedule

The game timer counts down from **30:00** to **0:00**. Jungle camps spawn every 5 minutes:

```
30:00  25:00  20:00  15:00  10:00  5:00
```

Announcements play 60s, 40s, and 20s before each spawn mark. No announcement plays at the **30:00** start (there's nothing before it) or at **0:00** (game over).

## Zeal schedule

Zeal spawns 15 seconds after game start (**29:45** on the clock) and then every 3:05, down to **08:10**:

```
29:45  26:40  23:35  20:30  17:25  14:20  11:15  8:10
```

`Zeal.wav` plays right at each mark (no advance warnings). If a zeal ever coincides with a jungle warning (the current timings don't overlap), the warning plays first and the zeal sound right after.

By default zeal announcements go to the same target as the jungle warnings. To whisper zeal to a different server group, use:

```bash
--zeal-server-group-id 13
```

(In the GUI, fill in the "Zeal group ID" field in the Whisper section; empty means same group as jungle.)

## Permissions

The bot needs permission to:

- Join its channel
- Speak in that channel
- Whisper to the target server group
- Satisfy whisper power requirements (if your server uses them)

If the bot prints `can_send_audio = false`, check that it isn't muted or away and has enough talk power.
