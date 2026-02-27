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

/// Connect to voice-echo's /discord-stream WebSocket and spawn bridge tasks.
///
/// Returns a BridgeHandle for sending outgoing messages (audio frames, control)
/// and an mpsc receiver for complete TTS mu-law buffers (buffered until mark).
///
/// When either the send or receive loop exits (WebSocket closes, error, etc.),
/// `on_disconnect` is notified so the caller can schedule a reconnect.
pub async fn connect(
    ws_url: &str,
    guild_id: &str,
    channel_id: &str,
    user_id: &str,
    on_disconnect: Arc<Notify>,
) -> Result<(BridgeHandle, mpsc::Receiver<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
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

    // Channel for complete TTS buffers (WS → playback task)
    let (tts_tx, tts_rx) = mpsc::channel::<Vec<u8>>(4);

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

    // Receive loop: reads from WS, buffers audio, sends complete TTS responses
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
                        if tts_tx.send(audio_buffer.clone()).await.is_err() {
                            tracing::error!("Playback channel closed");
                            break;
                        }
                        audio_buffer.clear();
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

    Ok((BridgeHandle { tx: outgoing_tx }, tts_rx))
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
