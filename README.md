# wwmts3gw

TeamSpeak 3 jungle timer bot for Where Winds Meet guild wars. Plays audio announcements 60, 30, and 15 seconds before each jungle camp spawn. The game timer counts down from 30:00 with spawns every 5 minutes.

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

Place the three announcements in the `assets/` directory (or anywhere, use the flags below):

| File | When it plays |
|---|---|
| `assets/Jungle 60 sec.wav` | 60 seconds before spawn |
| `assets/Jungle 30 sec.wav` | 30 seconds before spawn |
| `assets/Jungle 15 sec.wav` | 15 seconds before spawn |

WAV format, any sample rate (48 kHz recommended). Use the `--warn-*` flags to override paths:

```bash
--warn-60s /path/to/60s.wav
--warn-30s /path/to/30s.wav
--warn-15s /path/to/15s.wav
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

Announcements play 60s, 30s, and 15s before each spawn mark. No announcement plays at the **30:00** start (there's nothing before it) or at **0:00** (game over).

## Permissions

The bot needs permission to:

- Join its channel
- Speak in that channel
- Whisper to the target server group
- Satisfy whisper power requirements (if your server uses them)

If the bot prints `can_send_audio = false`, check that it isn't muted or away and has enough talk power.
