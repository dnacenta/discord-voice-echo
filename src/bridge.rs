use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite;

use crate::codec;

/// Handle for sending messages to voice-echo via the bridge WebSocket.
#[derive(Clone)]
pub struct BridgeHandle {
    pub tx: mpsc::UnboundedSender<String>,
}

/// Commands sent from the bridge receive loop to the playback loop.
pub enum PlaybackCommand {
    /// Complete TTS response (mu-law bytes) ready for playback.
    PlayTts(Vec<u8>),
    /// Start looping hold music.
    HoldStart,
    /// Stop hold music.
    HoldStop,
}

/// Connect to voice-echo's /discord-stream WebSocket and spawn bridge tasks.
///
/// Returns a BridgeHandle for sending outgoing messages (audio frames, control)
/// and an mpsc receiver for playback commands (TTS buffers + hold music control).
///
/// When either the send or receive loop exits (WebSocket closes, error, etc.),
/// `on_disconnect` is notified so the caller can schedule a reconnect.
pub async fn connect(
    ws_url: &str,
    guild_id: &str,
    channel_id: &str,
    user_id: &str,
    on_disconnect: Arc<Notify>,
) -> Result<(BridgeHandle, mpsc::Receiver<PlaybackCommand>), Box<dyn std::error::Error + Send + Sync>>
{
    tracing::info!(url = ws_url, "Connecting to voice-echo");

    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    tracing::info!("Connected to voice-echo WebSocket");

    // Send join event
    let join_msg = serde_json::json!({
        "type": "join",
        "guild_id": guild_id,
        "channel_id": channel_id,
        "user_id": user_id,
    });
    ws_write
        .send(tungstenite::Message::Text(join_msg.to_string().into()))
        .await?;
    tracing::info!("Sent join event to voice-echo");

    // Channel for outgoing messages (AudioReceiver → WS, and mark-back → WS)
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();

    // Channel for playback commands (WS → playback task)
    let (cmd_tx, cmd_rx) = mpsc::channel::<PlaybackCommand>(8);

    // Send loop: reads from outgoing channel, writes to WS
    let send_handle = tokio::spawn(async move {
        while let Some(msg) = outgoing_rx.recv().await {
            if let Err(e) = ws_write.send(tungstenite::Message::Text(msg.into())).await {
                tracing::error!("Bridge send error: {e}");
                break;
            }
        }
        tracing::info!("Bridge send loop exited");
    });

    // Receive loop: reads from WS, buffers audio, sends playback commands
    let recv_handle = tokio::spawn(async move {
        let mut audio_buffer: Vec<u8> = Vec::new();

        while let Some(msg) = ws_read.next().await {
            let text = match msg {
                Ok(tungstenite::Message::Text(t)) => t.to_string(),
                Ok(tungstenite::Message::Close(_)) => {
                    tracing::info!("voice-echo closed WebSocket");
                    break;
                }
                Err(e) => {
                    tracing::error!("Bridge receive error: {e}");
                    break;
                }
                _ => continue,
            };

            let event: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Failed to parse voice-echo message: {e}");
                    continue;
                }
            };

            match event["type"].as_str() {
                Some("audio") => {
                    if let Some(b64) = event["audio"].as_str() {
                        match base64::engine::general_purpose::STANDARD.decode(b64) {
                            Ok(mulaw) => {
                                audio_buffer.extend_from_slice(&mulaw);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decode TTS audio: {e}");
                            }
                        }
                    }
                }
                Some("mark") => {
                    if !audio_buffer.is_empty() {
                        tracing::info!(
                            mulaw_bytes = audio_buffer.len(),
                            "TTS response buffered, sending to playback"
                        );
                        if cmd_tx
                            .send(PlaybackCommand::PlayTts(audio_buffer.clone()))
                            .await
                            .is_err()
                        {
                            tracing::error!("Playback channel closed");
                            break;
                        }
                        audio_buffer.clear();
                    }
                }
                Some("hold_start") => {
                    tracing::debug!("Received hold_start from voice-echo");
                    if cmd_tx.send(PlaybackCommand::HoldStart).await.is_err() {
                        tracing::error!("Playback channel closed");
                        break;
                    }
                }
                Some("hold_stop") => {
                    tracing::debug!("Received hold_stop from voice-echo");
                    if cmd_tx.send(PlaybackCommand::HoldStop).await.is_err() {
                        tracing::error!("Playback channel closed");
                        break;
                    }
                }
                other => {
                    tracing::debug!(event_type = ?other, "Unhandled voice-echo event");
                }
            }
        }

        tracing::info!("Bridge receive loop exited");
    });

    // Monitor: notify caller when either bridge task exits
    tokio::spawn(async move {
        tokio::select! {
            _ = send_handle => {}
            _ = recv_handle => {}
        }
        on_disconnect.notify_one();
    });

    Ok((BridgeHandle { tx: outgoing_tx }, cmd_rx))
}

/// Encode a mu-law audio chunk as a JSON message for voice-echo.
pub fn audio_message(mulaw: &[u8], ssrc: u32, user_id: Option<u64>) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(mulaw);
    let mut msg = serde_json::json!({
        "type": "audio",
        "user_ssrc": ssrc,
        "audio": b64,
    });
    if let Some(uid) = user_id {
        msg["user_id"] = serde_json::json!(uid.to_string());
    }
    msg.to_string()
}

/// Encode user audio (mono i16 48kHz) for voice-echo (mu-law 8kHz).
///
/// Resamples 48kHz → 8kHz, encodes to mu-law, returns the JSON message string.
pub fn encode_user_audio(pcm_48k_mono: &[i16], ssrc: u32, user_id: Option<u64>) -> String {
    let pcm_8k = codec::resample_linear(pcm_48k_mono, 48000, 8000);
    let mulaw = codec::encode_mulaw(&pcm_8k);
    audio_message(&mulaw, ssrc, user_id)
}

/// Decode a complete TTS mu-law buffer into f32 stereo bytes for songbird.
///
/// Decodes mu-law → i16 PCM 8kHz → resample to 48kHz → f32 stereo bytes.
pub fn decode_tts_for_playback(mulaw: &[u8]) -> Vec<u8> {
    let pcm_8k = codec::decode_mulaw(mulaw);
    let pcm_48k = codec::resample_linear(&pcm_8k, 8000, 48000);
    codec::mono_i16_to_stereo_f32_bytes(&pcm_48k)
}

/// Load a WAV file and convert to f32 stereo 48kHz bytes for songbird playback.
///
/// Handles any sample rate and channel count by resampling and up/downmixing.
pub fn load_hold_music(path: &str, volume: f32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();

    tracing::info!(
        path = path,
        channels = spec.channels,
        sample_rate = spec.sample_rate,
        bits = spec.bits_per_sample,
        "Loading hold music"
    );

    // Read all samples as i16
    let samples: Vec<i16> = match spec.sample_format {
        hound::SampleFormat::Int => reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect(),
    };

    // Downmix to mono if stereo
    let mono: Vec<i16> = if spec.channels == 2 {
        codec::downmix_stereo(&samples)
    } else {
        samples
    };

    // Apply volume
    let scaled: Vec<i16> = mono
        .iter()
        .map(|&s| (s as f32 * volume).clamp(-32768.0, 32767.0) as i16)
        .collect();

    // Resample to 48kHz
    let pcm_48k = codec::resample_linear(&scaled, spec.sample_rate, 48000);

    // Convert to f32 stereo bytes for songbird
    let bytes = codec::mono_i16_to_stereo_f32_bytes(&pcm_48k);

    tracing::info!(
        pcm_bytes = bytes.len(),
        duration_secs = pcm_48k.len() as f32 / 48000.0,
        "Hold music loaded"
    );

    Ok(bytes)
}
