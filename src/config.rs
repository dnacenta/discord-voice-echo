use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    pub bridge: BridgeConfig,
}

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub guild_id: u64,
    pub voice_channel_id: u64,
}

#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    /// WebSocket URL for voice-echo's discord-stream endpoint.
    pub voice_echo_ws: String,
    /// Path to hold music WAV file (played while Echo processes a response).
    pub hold_music_file: Option<String>,
    /// Hold music volume (0.0–1.0). Default: 0.3
    #[serde(default = "default_hold_music_volume")]
    pub hold_music_volume: f32,
}

fn default_hold_music_volume() -> f32 {
    0.3
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let home = std::env::var("HOME").map_err(|_| "HOME not set")?;
        let config_dir = std::path::PathBuf::from(home).join(".discord-voice-echo");

        // Load .env if present
        let env_path = config_dir.join(".env");
        if env_path.exists() {
            dotenvy::from_path(&env_path).ok();
        }

        let config_path = config_dir.join("config.toml");
        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read {}: {e}", config_path.display()))?;

        let mut config: Config =
            toml::from_str(&content).map_err(|e| format!("Failed to parse config: {e}"))?;

        // Allow env override for bot token
        if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
            config.discord.bot_token = token;
        }

        Ok(config)
    }
}
