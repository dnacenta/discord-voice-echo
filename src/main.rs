mod bridge;
mod codec;
mod config;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use songbird::events::{CoreEvent, Event as SbEvent, EventContext, EventHandler as SbEventHandler};
use songbird::input::RawAdapter;
use songbird::shards::TwilightMap;
use songbird::Songbird;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use twilight_model::id::Id;

use config::Config;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--version") => println!("discord-voice {VERSION}"),
        Some("--help") | Some("-h") => print_usage(),
        Some(other) => {
            eprintln!("Unknown option: {other}");
            print_usage();
            std::process::exit(1);
        }
        None => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(run());
        }
    }
}

fn print_usage() {
    println!("discord-voice {VERSION}");
    println!("Discord voice sidecar for voice-echo");
    println!();
    println!("Usage: discord-voice [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --version   Print version");
    println!("  --help, -h  Print this help message");
}

async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "discord_voice=info".into()),
        )
        .init();

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let guild_id: Id<GuildMarker> = Id::new(config.discord.guild_id);
    let voice_channel_id: Id<ChannelMarker> = Id::new(config.discord.voice_channel_id);

    tracing::info!(
        guild_id = config.discord.guild_id,
        voice_channel_id = config.discord.voice_channel_id,
        "Starting discord-voice"
    );

    // Create twilight gateway shard
    let intents = Intents::GUILDS | Intents::GUILD_VOICE_STATES;
    let mut shard = Shard::new(ShardId::ONE, config.discord.bot_token.clone(), intents);
    let shard_number = shard.id().number();
    let shard_sender = shard.sender();

    // Songbird + bot identity — initialized on Ready event
    let mut songbird: Option<Arc<Songbird>> = None;
    let mut bot_user_id: Option<Id<UserMarker>> = None;

    // Track non-bot users in our voice channel
    let mut channel_users: HashSet<Id<UserMarker>> = HashSet::new();
    let mut bot_in_channel = false;

    // SSRC → Discord user ID mapping (populated by SpeakingStateUpdate events)
    let ssrc_map: Arc<RwLock<HashMap<u32, u64>>> = Arc::new(RwLock::new(HashMap::new()));

    // Shared bridge sender — swappable on reconnect without re-registering handlers
    let shared_tx: Arc<SharedBridgeTx> = Arc::new(SharedBridgeTx::new());

    // Bridge state
    let mut bridge_handle: Option<bridge::BridgeHandle> = None;
    let mut bridge_disconnect = Arc::new(Notify::new());
    let mut call_lock: Option<Arc<tokio::sync::Mutex<songbird::Call>>> = None;
    let mut playback_task: Option<JoinHandle<()>> = None;
    let playing = Arc::new(AtomicBool::new(false));

    // Reconnect state
    let mut reconnect_at: Option<Instant> = None;
    let mut reconnect_backoff = Duration::from_secs(1);

    // Shutdown signals
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("Failed to register SIGTERM handler");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("Failed to register SIGINT handler");

    tracing::info!("Connecting to Discord gateway...");

    loop {
        tokio::select! {
            biased;

            // Gateway events (highest priority)
            item = shard.next_event(EventTypeFlags::all()) => {
                let event = match item {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        tracing::warn!("Gateway error: {e:?}");
                        continue;
                    }
                    None => {
                        tracing::info!("Gateway connection closed");
                        break;
                    }
                };

                // Forward all events to songbird for voice connection management
                if let Some(ref sb) = songbird {
                    sb.process(&event).await;
                }

                match &event {
                    Event::Ready(ready) => {
                        let uid = ready.user.id;
                        tracing::info!(
                            user_id = uid.get(),
                            name = %ready.user.name,
                            "Connected to Discord"
                        );
                        bot_user_id = Some(uid);

                        // Create songbird now that we have the bot's user ID
                        let senders = TwilightMap::new(
                            [(shard_number, shard_sender.clone())].into_iter().collect(),
                        );
                        songbird =
                            Some(Arc::new(Songbird::twilight(Arc::new(senders), uid)));
                        tracing::info!("Songbird voice driver initialized");
                    }

                    Event::VoiceStateUpdate(vs) => {
                        let user_id = vs.user_id;

                        // Skip our own voice state updates
                        if bot_user_id == Some(user_id) {
                            tracing::debug!("Own voice state update, skipping");
                            continue;
                        }

                        // Check guild match
                        if vs.guild_id != Some(guild_id) {
                            continue;
                        }

                        let in_our_channel = vs.channel_id == Some(voice_channel_id);

                        if in_our_channel {
                            if channel_users.insert(user_id) {
                                tracing::info!(
                                    user_id = user_id.get(),
                                    "User joined voice channel"
                                );
                            }
                        } else if channel_users.remove(&user_id) {
                            tracing::info!(
                                user_id = user_id.get(),
                                "User left voice channel"
                            );
                        }

                        // Join if users present and we're not in
                        if !channel_users.is_empty() && !bot_in_channel {
                            if let Some(ref sb) = songbird {
                                tracing::info!("Users detected, joining voice channel");
                                match sb.join(guild_id, voice_channel_id).await {
                                    Ok(cl) => {
                                        tracing::info!("Joined voice channel");
                                        bot_in_channel = true;

                                        // Register event handlers for this Call
                                        {
                                            let mut call = cl.lock().await;
                                            call.add_global_event(
                                                SbEvent::Core(CoreEvent::VoiceTick),
                                                AudioReceiver {
                                                    shared_tx: Arc::clone(&shared_tx),
                                                    playing: Arc::clone(&playing),
                                                    ssrc_map: Arc::clone(&ssrc_map),
                                                },
                                            );
                                            call.add_global_event(
                                                SbEvent::Core(CoreEvent::SpeakingStateUpdate),
                                                SsrcMapper {
                                                    ssrc_map: Arc::clone(&ssrc_map),
                                                },
                                            );
                                        }
                                        tracing::info!("Audio receiver and SSRC mapper registered");

                                        call_lock = Some(cl.clone());

                                        // Connect bridge to voice-echo
                                        connect_bridge(
                                            &config,
                                            &cl,
                                            &bot_user_id,
                                            &shared_tx,
                                            &playing,
                                            &mut bridge_handle,
                                            &mut bridge_disconnect,
                                            &mut playback_task,
                                            &mut reconnect_backoff,
                                            &mut reconnect_at,
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to join voice channel: {e:?}"
                                        );
                                    }
                                }
                            }
                        }

                        // Leave if no users and we're in
                        if channel_users.is_empty() && bot_in_channel {
                            if let Some(ref sb) = songbird {
                                tracing::info!("Channel empty, leaving voice channel");

                                // Send leave event to voice-echo
                                if let Some(ref handle) = bridge_handle {
                                    handle
                                        .tx
                                        .send(r#"{"type":"leave"}"#.to_string())
                                        .ok();
                                }

                                if let Err(e) = sb.leave(guild_id).await {
                                    tracing::error!(
                                        "Failed to leave voice channel: {e:?}"
                                    );
                                }
                                bot_in_channel = false;
                                bridge_handle = None;
                                call_lock = None;
                                shared_tx.clear();
                                playing.store(false, Ordering::Relaxed);
                                reconnect_at = None;
                                reconnect_backoff = Duration::from_secs(1);
                                ssrc_map.write().unwrap().clear();

                                if let Some(task) = playback_task.take() {
                                    task.abort();
                                }
                            }
                        }
                    }

                    Event::GatewayReconnect => {
                        tracing::info!("Gateway reconnecting");
                    }

                    Event::Resumed => {
                        tracing::info!("Gateway resumed");
                    }

                    _ => {}
                }
            }

            // Bridge disconnected — schedule reconnect
            _ = bridge_disconnect.notified() => {
                if bridge_handle.is_some() && bot_in_channel {
                    tracing::warn!("Bridge disconnected, scheduling reconnect");
                    bridge_handle = None;
                    shared_tx.clear();
                    playing.store(false, Ordering::Relaxed);

                    if let Some(task) = playback_task.take() {
                        task.abort();
                    }

                    reconnect_at = Some(Instant::now() + reconnect_backoff);
                    tracing::info!(
                        backoff_secs = reconnect_backoff.as_secs(),
                        "Will attempt bridge reconnect"
                    );
                }
            }

            // Reconnect timer
            _ = async {
                match reconnect_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                reconnect_at = None;

                if !bot_in_channel {
                    continue;
                }

                if let Some(ref cl) = call_lock {
                    tracing::info!("Attempting bridge reconnect...");
                    connect_bridge(
                        &config,
                        cl,
                        &bot_user_id,
                        &shared_tx,
                        &playing,
                        &mut bridge_handle,
                        &mut bridge_disconnect,
                        &mut playback_task,
                        &mut reconnect_backoff,
                        &mut reconnect_at,
                    )
                    .await;
                }
            }

            // Shutdown: SIGINT
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down");
                break;
            }

            // Shutdown: SIGTERM
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down");
                break;
            }
        }
    }

    // Graceful shutdown
    tracing::info!("Shutting down gracefully...");

    // Notify voice-echo we're leaving
    if let Some(ref handle) = bridge_handle {
        handle.tx.send(r#"{"type":"leave"}"#.to_string()).ok();
    }

    // Leave voice channel
    if bot_in_channel {
        if let Some(ref sb) = songbird {
            if let Err(e) = sb.leave(guild_id).await {
                tracing::error!("Failed to leave voice channel during shutdown: {e:?}");
            } else {
                tracing::info!("Left voice channel");
            }
        }
    }

    // Abort playback
    if let Some(task) = playback_task.take() {
        task.abort();
    }

    tracing::info!("Shutdown complete");
}

/// Connect bridge to voice-echo and set up audio pipeline.
///
/// On success: sets shared_tx, spawns playback task, resets backoff.
/// On failure: schedules a reconnect with exponential backoff.
#[allow(clippy::too_many_arguments)]
async fn connect_bridge(
    config: &Config,
    call_lock: &Arc<tokio::sync::Mutex<songbird::Call>>,
    bot_user_id: &Option<Id<UserMarker>>,
    shared_tx: &Arc<SharedBridgeTx>,
    playing: &Arc<AtomicBool>,
    bridge_handle: &mut Option<bridge::BridgeHandle>,
    bridge_disconnect: &mut Arc<Notify>,
    playback_task: &mut Option<JoinHandle<()>>,
    reconnect_backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
) {
    let uid_str = bot_user_id.map(|u| u.get().to_string()).unwrap_or_default();

    // Fresh disconnect notifier for this connection
    let new_disconnect = Arc::new(Notify::new());

    match bridge::connect(
        &config.bridge.voice_echo_ws,
        &config.discord.guild_id.to_string(),
        &config.discord.voice_channel_id.to_string(),
        &uid_str,
        Arc::clone(&new_disconnect),
    )
    .await
    {
        Ok((handle, tts_rx)) => {
            // Set the shared sender so AudioReceiver can use it
            shared_tx.set(handle.tx.clone());
            tracing::info!("Audio receiver connected to bridge");

            // Spawn TTS playback task
            let play_call = call_lock.clone();
            let play_tx = handle.tx.clone();
            let play_flag = Arc::clone(playing);
            *playback_task = Some(tokio::spawn(playback_loop(
                tts_rx, play_call, play_tx, play_flag,
            )));

            *bridge_handle = Some(handle);
            *bridge_disconnect = new_disconnect;
            *reconnect_backoff = Duration::from_secs(1);
            *reconnect_at = None;

            tracing::info!("Bridge connected successfully");
        }
        Err(e) => {
            tracing::error!("Failed to connect bridge: {e}");
            *reconnect_backoff = (*reconnect_backoff * 2).min(MAX_RECONNECT_BACKOFF);
            *reconnect_at = Some(Instant::now() + *reconnect_backoff);
            tracing::info!(
                backoff_secs = reconnect_backoff.as_secs(),
                "Will retry bridge connection"
            );
        }
    }
}

/// Shared bridge sender that can be swapped on reconnect.
///
/// AudioReceiver holds an Arc to this. When the bridge reconnects, the sender
/// is replaced without needing to re-register the songbird event handler.
struct SharedBridgeTx {
    inner: RwLock<Option<mpsc::UnboundedSender<String>>>,
}

impl SharedBridgeTx {
    fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    fn set(&self, tx: mpsc::UnboundedSender<String>) {
        *self.inner.write().unwrap() = Some(tx);
    }

    fn clear(&self) {
        *self.inner.write().unwrap() = None;
    }

    fn send(&self, msg: String) -> bool {
        if let Some(ref tx) = *self.inner.read().unwrap() {
            tx.send(msg).is_ok()
        } else {
            false
        }
    }
}

/// Audio receiver: captures decoded voice from Discord users,
/// resamples to 8kHz mu-law, and forwards to voice-echo via bridge.
///
/// Uses SharedBridgeTx so the sender can be swapped on reconnect
/// without re-registering this handler with songbird.
struct AudioReceiver {
    shared_tx: Arc<SharedBridgeTx>,
    playing: Arc<AtomicBool>,
    ssrc_map: Arc<RwLock<HashMap<u32, u64>>>,
}

#[async_trait]
impl SbEventHandler for AudioReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SbEvent> {
        if let EventContext::VoiceTick(tick) = ctx {
            // Skip sending while TTS is playing to avoid echo
            if self.playing.load(Ordering::Relaxed) {
                return None;
            }

            for (&ssrc, data) in &tick.speaking {
                if let Some(decoded) = &data.decoded_voice {
                    // Look up Discord user ID from SSRC
                    let user_id = self.ssrc_map.read().unwrap().get(&ssrc).copied();

                    // decoded is mono i16 PCM at 48kHz (~960 samples per 20ms tick)
                    let msg = bridge::encode_user_audio(decoded, ssrc, user_id);
                    if !self.shared_tx.send(msg) {
                        // Bridge not connected or channel closed — silently drop
                        return None;
                    }
                }
            }
        }
        None
    }
}

/// Maps SSRC → Discord user ID from songbird SpeakingStateUpdate events.
///
/// Discord assigns each voice participant an SSRC (synchronization source).
/// This handler captures the SSRC → user mapping so we can include user IDs
/// in audio frames sent to voice-echo.
struct SsrcMapper {
    ssrc_map: Arc<RwLock<HashMap<u32, u64>>>,
}

#[async_trait]
impl SbEventHandler for SsrcMapper {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SbEvent> {
        if let EventContext::SpeakingStateUpdate(update) = ctx {
            if let Some(uid) = update.user_id {
                let user_id = uid.0;
                self.ssrc_map.write().unwrap().insert(update.ssrc, user_id);
                tracing::debug!(ssrc = update.ssrc, user_id = user_id, "SSRC mapped to user");
            }
        }
        None
    }
}

/// TTS playback loop: receives complete mu-law buffers from bridge,
/// converts to f32 stereo 48kHz, plays in songbird, waits for completion,
/// then sends mark back to voice-echo.
async fn playback_loop(
    mut tts_rx: mpsc::Receiver<Vec<u8>>,
    call_lock: Arc<tokio::sync::Mutex<songbird::Call>>,
    ws_tx: mpsc::UnboundedSender<String>,
    playing: Arc<AtomicBool>,
) {
    while let Some(mulaw_buffer) = tts_rx.recv().await {
        tracing::info!(
            mulaw_bytes = mulaw_buffer.len(),
            "Playing TTS response in Discord"
        );

        // Suppress audio sending during playback
        playing.store(true, Ordering::Relaxed);

        // Convert: mu-law 8kHz mono → f32 48kHz stereo bytes
        let f32_bytes = bridge::decode_tts_for_playback(&mulaw_buffer);

        // Create songbird input from raw f32 PCM
        let cursor = std::io::Cursor::new(f32_bytes);
        let adapter = RawAdapter::new(cursor, 48000, 2);
        let input: songbird::input::Input = adapter.into();

        // Play — stop any previous track first
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let mut call = call_lock.lock().await;
            call.stop();
            let handle = call.play_input(input);

            // Register handler to notify when playback finishes
            if let Err(e) = handle.add_event(
                SbEvent::Track(songbird::events::TrackEvent::End),
                PlaybackDone {
                    tx: std::sync::Mutex::new(Some(done_tx)),
                },
            ) {
                tracing::warn!("Failed to register playback end handler: {e:?}");
            }
        }

        // Wait for playback to finish (or channel close)
        let _ = done_rx.await;

        tracing::debug!("TTS playback finished");
        playing.store(false, Ordering::Relaxed);

        // Send mark back to voice-echo so it resets VAD
        ws_tx.send(r#"{"type":"mark"}"#.to_string()).ok();
    }

    tracing::info!("Playback loop exited");
}

/// Songbird event handler that fires when a track finishes playing.
struct PlaybackDone {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait]
impl SbEventHandler for PlaybackDone {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<SbEvent> {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            tx.send(()).ok();
        }
        None
    }
}
