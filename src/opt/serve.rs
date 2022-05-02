use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use futures::{stream::FuturesUnordered, StreamExt};
use penumbra_crypto::{Value, Zero};
use std::{env, path::PathBuf, time::Duration};

use crate::{
    opt::ChannelIdAndMessageId, responder::RequestQueue, Catchup, Handler, Responder, Wallet,
};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
    /// The transaction fee for each response (paid in upenumbra).
    #[structopt(long, default_value = "0")]
    fee: u64,
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1day", parse(try_from_str = humantime::parse_duration))]
    rate_limit: Duration,
    /// Maximum number of times to reply to a user informing them of the rate limit.
    #[clap(long, default_value = "5")]
    reply_limit: usize,
    /// Interval at which to save the wallet state to disk.
    #[clap(long = "save", default_value = "1m", parse(try_from_str = humantime::parse_duration))]
    save_interval: Duration,
    /// An estimate of the duration for each block (this is used to tune sleeps when retrying
    /// various operations).
    #[clap(long, default_value = "10s", parse(try_from_str = humantime::parse_duration))]
    block_time_estimate: Duration,
    /// The number of times to retry when an error happens while communicating with the server.
    #[clap(long, default_value = "5")]
    sync_retries: u32,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(long, default_value = "1")]
    max_addresses: usize,
    /// Internal buffer size for the queue of actions to perform.
    #[clap(long, default_value = "100")]
    buffer_size: usize,
    /// Path to the wallet file to use [default: platform appdata directory].
    #[clap(long, short)]
    wallet_file: Option<PathBuf>,
    /// The address of the pd+tendermint node.
    #[clap(short, long, default_value = "testnet.penumbra.zone")]
    node: String,
    /// The port to use to speak to pd's wallet server.
    #[clap(long, default_value = "8080")]
    pd_port: u16,
    /// The port to use to speak to tendermint.
    #[clap(long, default_value = "26657")]
    rpc_port: u16,
    /// The source address index in the wallet to use when dispensing tokens (if unspecified uses
    /// any funds available).
    #[clap(long = "source")]
    source_address: Option<u64>,
    /// Message/channel ids to catch up on backlog from (can be specified as
    /// `<channel_id>/<message_id>` or a full URL as generated by Discord).
    #[clap(long)]
    catch_up: Vec<ChannelIdAndMessageId>,
    /// Batch size for responding to catch-up backlog.
    #[clap(long, default_value = "25")]
    catch_up_batch_size: usize,
    /// The amounts to send for each response, written as typed values 1.87penumbra, 12cubes, etc.
    values: Vec<Value>,
}

impl Serve {
    pub async fn exec(self) -> anyhow::Result<()> {
        if self.values.is_empty() {
            anyhow::bail!("at least one value must be provided");
        } else if self.values.iter().any(|v| v.amount.is_zero()) {
            anyhow::bail!("all values must be non-zero");
        }

        let discord_token =
            env::var("DISCORD_TOKEN").context("missing environment variable DISCORD_TOKEN")?;

        // Look up the path to the wallet file per platform, creating the directory if needed
        let wallet_file = self.wallet_file.map_or_else(
            || {
                let project_dir = ProjectDirs::from("zone", "penumbra", "pcli")
                    .expect("can access penumbra project dir");
                // Currently we use just the data directory. Create it if it is missing.
                std::fs::create_dir_all(project_dir.data_dir())
                    .expect("can create penumbra data directory");
                project_dir.data_dir().join("penumbra_wallet.json")
            },
            PathBuf::from,
        );

        // Make a worker to handle the wallet
        let (wallet_requests, wallet_ready, wallet) = Wallet::new(
            wallet_file,
            self.source_address,
            self.save_interval,
            self.block_time_estimate,
            self.buffer_size,
            self.sync_retries,
            self.node,
            self.pd_port,
            self.rpc_port,
        );

        // Make a worker to handle the address queue
        let (send_requests, responder) = Responder::new(
            wallet_requests,
            self.max_addresses,
            self.buffer_size,
            self.values,
            self.fee,
        );

        let handler = Handler::new(self.rate_limit, self.reply_limit, wallet_ready);

        // Make a new client using a token set by an environment variable, with our handlers
        let mut client = serenity::Client::builder(&discord_token)
            .event_handler(handler)
            .await?;

        // Put the sending end of the address queue into the global TypeMap
        client
            .data
            .write()
            .await
            .insert::<RequestQueue>(send_requests.clone());

        // Make a separate catch-up worker for each catch-up task, and collect their results (first
        // to fail kills the bot)
        let http = client.cache_and_http.http.clone();
        let catch_up = tokio::spawn(async move {
            let mut catch_ups: FuturesUnordered<_> = self
                .catch_up
                .into_iter()
                .map(
                    |ChannelIdAndMessageId {
                         channel_id,
                         message_id,
                     }| {
                        let catch_up = Catchup::new(
                            channel_id,
                            self.catch_up_batch_size,
                            http.clone(),
                            send_requests.clone(),
                        );
                        tokio::spawn(catch_up.run(message_id))
                    },
                )
                .collect();

            while let Some(result) = catch_ups.next().await {
                result??;
            }

            // Wait forever
            std::future::pending().await
        });

        // Start the client and the two workers
        tokio::select! {
            result = tokio::spawn(async move { client.start().await }) =>
                result.unwrap().context("error in discord client service"),
            result = tokio::spawn(async move { responder.run().await }) =>
                result.unwrap().context("error in responder service"),
            result = wallet.run() => result.context("error in wallet service"),
            result = catch_up => result.context("error in catchup service")?,
        }
    }
}
