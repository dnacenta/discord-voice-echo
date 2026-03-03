use discord_voice_echo::{config::Config, DiscordEcho};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--version") => println!("discord-voice-echo {VERSION}"),
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
    println!("discord-voice-echo {VERSION}");
    println!("Discord voice sidecar for voice-echo");
    println!();
    println!("Usage: discord-voice-echo [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --version   Print version");
    println!("  --help, -h  Print this help message");
}

async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "discord_voice_echo=info".into()),
        )
        .init();

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        guild_id = config.discord.guild_id,
        voice_channel_id = config.discord.voice_channel_id,
        "Starting discord-voice-echo"
    );

    let mut discord = DiscordEcho::new(config);
    if let Err(e) = discord.start().await {
        tracing::error!(error = %e, "Fatal error");
        std::process::exit(1);
    }
}
