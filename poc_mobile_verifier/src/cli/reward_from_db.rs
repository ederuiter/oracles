use crate::{
    heartbeats::Heartbeats, reward_share::get_scheduled_tokens, speedtests::SpeedtestAverages,
    subnetwork_rewards::SubnetworkRewards, Result, Settings,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use helium_crypto::PublicKey;
use serde_json::json;

/// Reward a period from the entries in the database
#[derive(Debug, clap::Args)]
pub struct Cmd {
    #[clap(long)]
    start: NaiveDateTime,
    #[clap(long)]
    end: NaiveDateTime,
}

impl Cmd {
    pub async fn run(self, settings: &Settings) -> Result {
        let Self { start, end } = self;

        let start = DateTime::from_utc(start, Utc);
        let end = DateTime::from_utc(end, Utc);

        tracing::info!("Rewarding shares from the following time range: {start} to {end}");
        let epoch = start..end;
        let expected_rewards = get_scheduled_tokens(epoch.start, epoch.end - epoch.start)
            .expect("Couldn't get expected rewards");

        let follower = settings.follower.connect_follower()?;
        let pool = settings.database.connect(10).await?;

        let heartbeats = Heartbeats::validated(&pool, epoch.start).await?;
        let speedtests = SpeedtestAverages::validated(&pool, epoch.end).await?;
        let rewards =
            SubnetworkRewards::from_epoch(follower, &epoch, heartbeats, speedtests).await?;

        let total_rewards = rewards
            .rewards
            .iter()
            .fold(0, |acc, reward| acc + reward.amount);
        let rewards: Vec<(PublicKey, u64)> = rewards
            .rewards
            .iter()
            .map(|r| {
                (
                    PublicKey::try_from(r.account.as_slice()).expect("unable to get public key"),
                    r.amount,
                )
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "rewards": rewards,
                "total_rewards": total_rewards,
                "expected_rewards": expected_rewards,
            }))?
        );

        Ok(())
    }
}
