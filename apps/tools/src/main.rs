use anyhow::Result;
use clap::{Parser, Subcommand};
use shared::domain::ChannelKind;
use storage::Storage;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(long, default_value = "sqlite://community.db")]
    database_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    CreateGuild {
        owner_user_id: i64,
        name: String,
    },
    CreateChannel {
        guild_id: i64,
        name: String,
        kind: String,
    },
    CreateInvite {
        guild_id: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let storage = Storage::new(&cli.database_url).await?;

    match cli.command {
        Command::CreateGuild {
            owner_user_id,
            name,
        } => {
            let guild_id = storage
                .create_guild(&name, shared::domain::UserId(owner_user_id))
                .await?;
            println!("created guild_id={}", guild_id.0);
        }
        Command::CreateChannel {
            guild_id,
            name,
            kind,
        } => {
            let kind = if kind.eq_ignore_ascii_case("voice") {
                ChannelKind::Voice
            } else {
                ChannelKind::Text
            };
            let channel_id = storage
                .create_channel(shared::domain::GuildId(guild_id), &name, kind)
                .await?;
            println!("created channel_id={}", channel_id.0);
        }
        Command::CreateInvite { guild_id } => {
            println!("placeholder invite for guild_id={guild_id}: INVITE-TODO");
        }
    }

    Ok(())
}
