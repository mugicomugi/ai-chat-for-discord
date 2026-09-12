//! Read-only installation check. Does not register commands or read/send messages.
use anyhow::{Context, Result, bail};
use discord_discussion_bot::config::Config;
use serde_json::Value;

async fn get(client: &reqwest::Client, token: &str, path: &str) -> Result<Value> {
    let response = client
        .get(format!("https://discord.com/api/v10{path}"))
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Discord transport failure"))?;
    let status = response.status();
    let body: Value = response.json().await.context("invalid Discord response")?;
    if !status.is_success() {
        bail!("Discord HTTP {} code {}", status.as_u16(), body["code"]);
    }
    Ok(body)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let config = Config::from_env()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let user = get(&client, &config.discord_token, "/users/@me").await?;
    println!("bot_id={}", user["id"].as_str().unwrap_or("unknown"));
    let guilds = get(
        &client,
        &config.discord_token,
        "/users/@me/guilds?limit=200",
    )
    .await?;
    let guilds = guilds.as_array().context("invalid guild list")?;
    println!("joined_guild_count={}", guilds.len());
    if let Some(expected) = config.guild_id {
        println!(
            "configured_guild_accessible={}",
            guilds
                .iter()
                .any(|guild| guild["id"].as_str() == Some(&expected.to_string()))
        );
    }
    let mut matches = 0;
    for guild in guilds {
        let id = guild["id"].as_str().context("missing guild ID")?;
        let roles = get(
            &client,
            &config.discord_token,
            &format!("/guilds/{id}/roles"),
        )
        .await?;
        if roles.as_array().is_some_and(|roles| {
            roles
                .iter()
                .any(|role| role["id"].as_str() == Some(&config.grant_role_id.to_string()))
        }) {
            println!("grant_role_guild_id={id}");
            matches += 1;
        }
    }
    if matches == 0 {
        bail!(
            "GRANT_ROLE_ID was not found in any joined guild; check the IDs and install the bot in the intended server"
        );
    }
    Ok(())
}
