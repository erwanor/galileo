use std::{borrow::Borrow, sync::Arc};

use penumbra_crypto::{Address, Value};
use regex::Regex;
use serenity::{
    model::channel::Message,
    prelude::{Mentionable, TypeMapKey},
    CacheAndHttp,
};
use tokio::sync::{mpsc, oneshot};

use crate::wallet;

pub struct Responder {
    /// Maximum number of addresses to handle per message.
    max_addresses: usize,
    /// Actions to perform.
    actions: mpsc::Receiver<Request>,
    /// Requests outbound to the wallet worker.
    requests: mpsc::Sender<wallet::Request>,
    /// Cache and http for dispatching replies.
    cache_http: Arc<CacheAndHttp>,
    /// Values to send each time.
    values: Vec<Value>,
}

/// `TypeMap` key for the address queue.
pub struct RequestQueue;

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for RequestQueue {
    type Value = mpsc::Sender<Request>;
}

/// `TypeMap` value for the sender end of the address queue.
#[derive(Debug)]
pub struct Request {
    /// The originating message that contained these addresses.
    message: Message,
    /// The addresses matched in the originating message.
    addresses: Vec<AddressOrAlmost>,
    /// The sender for the response.
    response: oneshot::Sender<(Message, String)>,
}

impl Request {
    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn try_from_message(
        message: Message,
    ) -> Result<(oneshot::Receiver<(Message, String)>, Request), Message> {
        let address_regex =
            Regex::new(r"penumbrav\dt1[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{126}").unwrap();

        // Collect all the matches into a struct, bundled with the original message
        tracing::trace!("collecting addresses from message");
        let addresses: Vec<AddressOrAlmost> = address_regex
            .find_iter(&message.content)
            .map(|m| {
                use AddressOrAlmost::*;
                match m.as_str().parse() {
                    Ok(addr) => Address(Box::new(addr)),
                    Err(e) => {
                        tracing::trace!(error = ?e, "failed to parse address");
                        Almost(m.as_str().to_string())
                    }
                }
            })
            .collect();

        // If no addresses were found, don't bother sending the message to the queue
        if addresses.is_empty() {
            Err(message)
        } else {
            let (tx, rx) = oneshot::channel();
            Ok((
                rx,
                Request {
                    message,
                    addresses,
                    response: tx,
                },
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub enum AddressOrAlmost {
    Address(Box<Address>),
    Almost(String),
}

impl Responder {
    pub fn new(
        requests: mpsc::Sender<wallet::Request>,
        max_addresses: usize,
        cache_http: Arc<CacheAndHttp>,
        buffer_size: usize,
        values: Vec<Value>,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            tx,
            Responder {
                requests,
                max_addresses,
                actions: rx,
                cache_http,
                values,
            },
        )
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(Request {
            addresses,
            message,
            response,
        }) = self.actions.recv().await
        {
            let reply = self.dispense(&message, addresses).await?;
            let _ = response.send((message, reply));
        }

        Ok(())
    }

    async fn dispense(
        &mut self,
        message: impl Borrow<Message>,
        mut addresses: Vec<AddressOrAlmost>,
    ) -> anyhow::Result<String> {
        let message = message.borrow();

        // Track addresses to which we successfully dispensed tokens
        let mut succeeded_addresses = Vec::<Address>::new();

        // Track addresses (and associated errors) which we tried to send tokens to, but failed
        let mut failed_addresses = Vec::<(Address, String)>::new();

        // Track addresses which couldn't be parsed
        let mut unparsed_addresses = Vec::<String>::new();

        // Extract up to the maximum number of permissible valid addresses from the list
        let mut count = 0;
        while count <= self.max_addresses {
            count += 1;
            match addresses.pop() {
                Some(AddressOrAlmost::Address(addr)) => {
                    // Reply to the originating message with the address
                    tracing::info!(user_name = ?message.author.name, user_id = ?message.author.id.to_string(), address = ?addr, "sending tokens");

                    let (result, request) = wallet::Request::send(*addr, self.values.clone());
                    self.requests.send(request).await?;

                    // TODO: While this is happening, use the typing indicator API to show that
                    // something is happening.

                    match result.await? {
                        Ok(()) => succeeded_addresses.push(*addr),
                        Err(e) => failed_addresses.push((*addr, e.to_string())),
                    }
                }
                Some(AddressOrAlmost::Almost(addr)) => {
                    unparsed_addresses.push(addr);
                }
                None => break,
            }
        }

        // Separate the rest of the list into unparsed and remaining valid ones
        let mut remaining_addresses = Vec::<Address>::new();
        for addr in addresses {
            match addr {
                AddressOrAlmost::Address(addr) => remaining_addresses.push(*addr),
                AddressOrAlmost::Almost(addr) => unparsed_addresses.push(addr),
            }
        }

        // Reply with a summary of what occurred
        let response = self
            .reply_dispense_summary(
                &succeeded_addresses,
                &failed_addresses,
                &unparsed_addresses,
                &remaining_addresses,
                message,
            )
            .await;

        Ok(response)
    }

    async fn reply_dispense_summary<'a>(
        &mut self,
        succeeded_addresses: &[Address],
        failed_addresses: &[(Address, String)],
        unparsed_addresses: &[String],
        remaining_addresses: &[Address],
        message: impl Borrow<Message>,
    ) -> String {
        let succeeded_addresses = succeeded_addresses.borrow();
        let failed_addresses = failed_addresses.borrow();
        let remaining_addresses = remaining_addresses.borrow();

        let mut response = String::new();

        if !succeeded_addresses.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for addr in succeeded_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !failed_addresses.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:\n");
            for (addr, error) in failed_addresses {
                response.push_str(&format!("\n`{}` (error: {})", addr, error));
            }

            // Construct a mention for the admin roles for this server
            let mention_admins = if let Some(guild_id) = message.borrow().guild_id {
                self.cache_http
                    .cache
                    .guild_roles(guild_id)
                    .await
                    .iter()
                    .map(IntoIterator::into_iter)
                    .flatten()
                    .filter(|(_, r)| r.permissions.administrator())
                    .map(|(&id, _)| id)
                    .map(|role_id| role_id.mention().to_string())
                    .collect::<Vec<String>>()
                    .join(" ")
            } else {
                "Admin(s)".to_string()
            };

            response.push_str(&format!(
                "\n{mention_admins}: you may want to investigate this error :)",
            ))
        }

        if !unparsed_addresses.is_empty() {
            response.push_str(
                "\nThe following _look like_ Penumbra addresses, \
                but are invalid (maybe a typo or old address version?):",
            );
            for addr in unparsed_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !remaining_addresses.is_empty() {
            response.push_str(&format!(
                "\nI'm only allowed to send tokens to addresses {} at a time; \
                try again later to get tokens for the following addresses:",
                self.max_addresses,
            ));
            for addr in remaining_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        response
    }
}
