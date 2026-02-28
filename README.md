# discord-voice-echo

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/discord-voice-echo)](LICENSE)
[![Version](https://img.shields.io/github/v/tag/dnacenta/discord-voice-echo?label=version&color=green)](https://github.com/dnacenta/discord-voice-echo/tags)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange)](https://rustup.rs/)

Discord voice channel sidecar for [voice-echo](https://github.com/dnacenta/voice-echo). Lets users talk to Echo directly in a Discord voice channel using the same pipeline (VAD → STT → Claude → TTS) that powers phone calls.

## How It Works

discord-voice-echo is a standalone Rust binary that bridges Discord voice audio to voice-echo's existing pipeline via a local WebSocket. It handles all codec conversion so voice-echo doesn't need any Discord dependencies.

```
Discord Voice Channel          discord-voice-echo              voice-echo
┌─────────────────┐          ┌──────────────────┐          ┌──────────────┐
│  User speaks     │  Opus   │  Decode Opus      │  mu-law  │  VAD → STT   │
│  into mic        │ ──────> │  48kHz → 8kHz     │ ──────>  │  → Claude    │
│                  │         │  PCM → mu-law      │  (WS)   │  → TTS       │
│  Hears Echo      │  Opus   │  mu-law → PCM     │  mu-law  │              │
│  respond         │ <────── │  8kHz → 48kHz     │ <──────  │              │
└─────────────────┘          └──────────────────┘          └──────────────┘
```

**User experience:**
1. User joins `#talk-to-echo` voice channel
2. Echo's bot auto-joins when it detects a user
3. User speaks normally — Echo listens, transcribes, thinks, responds with TTS
4. Echo auto-leaves when the channel empties

## Requirements

- [voice-echo](https://github.com/dnacenta/voice-echo) running with the `/discord-stream` endpoint enabled
- A Discord bot with voice permissions (Connect, Speak, Use Voice Activity)
- Rust toolchain (for building from source)

## Installation

### From source

```bash
git clone https://github.com/dnacenta/discord-voice-echo.git
cd discord-voice-echo
cargo build --release
cp target/release/discord-voice-echo /usr/local/bin/
```

## Configuration

Create `~/.discord-voice-echo/config.toml`:

```toml
[discord]
bot_token = "your-bot-token-here"
guild_id = 123456789012345678
voice_channel_id = 123456789012345678

[bridge]
voice_echo_ws = "ws://127.0.0.1:8443/discord-stream"
```

The bot token can also be set via the `DISCORD_BOT_TOKEN` environment variable.

### Discord Bot Setup

1. Create a bot at [discord.com/developers](https://discord.com/developers/applications)
2. Enable the following gateway intents: **Server Members**, **Voice States**
3. Invite the bot to your server with permissions: **Connect**, **Speak**, **Use Voice Activity**
4. Create a voice channel for Echo (e.g., `#talk-to-echo`)
5. Copy the guild ID, voice channel ID, and bot token into the config

## Running

```bash
# Direct
discord-voice-echo

# With debug logging
RUST_LOG=discord_voice_echo=debug discord-voice-echo

# As a systemd service
sudo systemctl enable --now discord-voice
```

### Systemd Service

```ini
[Unit]
Description=discord-voice-echo — Discord voice sidecar for voice-echo
After=network.target voice-echo.service
Wants=voice-echo.service

[Service]
Type=simple
User=echo
WorkingDirectory=/home/echo
ExecStart=/usr/local/bin/discord-voice-echo
Environment=RUST_LOG=discord_voice_echo=info
Environment=HOME=/home/echo
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

## Features

- **Auto join/leave** — joins when users are present, leaves when empty
- **Auto-reconnect** — if voice-echo restarts, reconnects with exponential backoff (1s–30s)
- **SSRC mapping** — maps Discord audio streams to user IDs for multi-user identification
- **Graceful shutdown** — handles SIGTERM/SIGINT cleanly (leaves channel, closes bridge)
- **DAVE E2EE** — uses a [songbird fork](https://github.com/dnacenta/songbird/tree/dave) with Discord's end-to-end encryption support

## Architecture

discord-voice-echo is intentionally a separate binary from voice-echo:

- **Dependency isolation** — songbird pulls ~40 crates (crypto, Opus, UDP, RTP). Users who only want phone calls don't pay this cost.
- **Independent updates** — each binary updates on its own schedule.
- **Separation of concerns** — discord-voice-echo handles Discord protocol, voice-echo handles the audio pipeline.

### Protocol

Communication with voice-echo uses JSON over a local WebSocket (`/discord-stream`):

```
discord-voice-echo → voice-echo:
  {"type": "join", "guild_id": "...", "channel_id": "...", "user_id": "..."}
  {"type": "audio", "user_ssrc": 12345, "user_id": "...", "audio": "<base64 mu-law 8kHz>"}
  {"type": "mark"}
  {"type": "leave"}

voice-echo → discord-voice-echo:
  {"type": "audio", "audio": "<base64 mu-law 8kHz>"}
  {"type": "mark"}
```

## License

AGPL-3.0
