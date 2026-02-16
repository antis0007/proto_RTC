use anyhow::Result;
use clap::Parser;
use client_core::{CommunityClient, PassthroughCrypto};
use shared::domain::{ChannelId, GuildId};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    server_url: String,
    #[arg(long)]
    username: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    let mut client = CommunityClient::new(args.server_url, PassthroughCrypto);
    let user_id = client.login(&args.username).await?;
    println!("Logged in as user_id={user_id}");

    println!("Guilds/channels listing over WS is TODO in this minimal skeleton.");

    let msg = client.send_message_request(ChannelId(1), "hello world");
    println!(
        "Prepared send_message payload: {}",
        serde_json::to_string(&msg)?
    );

    let token_req = client.request_livekit_token(GuildId(1), ChannelId(2), true, false);
    println!(
        "Prepared LiveKit token request payload: {}",
        serde_json::to_string(&token_req)?
    );
    println!("LiveKit Rust SDK client connect step is TODO (placeholder: connected).");

    Ok(())
}
