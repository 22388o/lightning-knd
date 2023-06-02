use std::collections::hash_map::Entry;

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;

use bitcoin::secp256k1::Secp256k1;

use crate::database::WalletDatabase;
use api::lightning::chain::chaininterface::{
    BroadcasterInterface, ConfirmationTarget, FeeEstimator,
};
use api::lightning::chain::keysinterface::KeysManager;
use api::lightning::events::{Event, PaymentPurpose};
use api::lightning::routing::gossip::NodeId;
use hex::ToHex;
use log::{error, info};
use rand::{thread_rng, Rng};
use tokio::runtime::Handle;

use crate::bitcoind::BitcoindClient;
use crate::ldk::ldk_error;
use crate::ldk::payment_info::{HTLCStatus, MillisatAmount, PaymentInfo};
use crate::wallet::{Wallet, WalletInterface};

use super::controller::AsyncAPIRequests;
use super::payment_info::PaymentInfoStorage;
use super::{ChannelManager, NetworkGraph};

pub(crate) struct EventHandler {
    channel_manager: Arc<ChannelManager>,
    bitcoind_client: Arc<BitcoindClient>,
    keys_manager: Arc<KeysManager>,
    inbound_payments: PaymentInfoStorage,
    outbound_payments: PaymentInfoStorage,
    network_graph: Arc<NetworkGraph>,
    wallet: Arc<Wallet<WalletDatabase, BitcoindClient>>,
    async_api_requests: Arc<AsyncAPIRequests>,
    runtime_handle: Handle,
}

impl EventHandler {
    // TODO remove when payments storage is in database
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_manager: Arc<ChannelManager>,
        bitcoind_client: Arc<BitcoindClient>,
        keys_manager: Arc<KeysManager>,
        inbound_payments: PaymentInfoStorage,
        outbound_payments: PaymentInfoStorage,
        network_graph: Arc<NetworkGraph>,
        wallet: Arc<Wallet<WalletDatabase, BitcoindClient>>,
        async_api_requests: Arc<AsyncAPIRequests>,
        runtime_handle: Handle,
    ) -> EventHandler {
        EventHandler {
            channel_manager,
            bitcoind_client,
            keys_manager,
            inbound_payments,
            outbound_payments,
            network_graph,
            wallet,
            async_api_requests,
            runtime_handle,
        }
    }
}

impl api::lightning::events::EventHandler for EventHandler {
    fn handle_event(&self, event: api::lightning::events::Event) {
        tokio::task::block_in_place(move || {
            self.runtime_handle.block_on(self.handle_event_async(event))
        })
    }
}

impl EventHandler {
    pub async fn handle_event_async(&self, event: api::lightning::events::Event) {
        match event {
            Event::FundingGenerationReady {
                temporary_channel_id,
                counterparty_node_id,
                channel_value_satoshis,
                output_script,
                user_channel_id,
            } => {
                let (fee_rate, respond) = match self
                    .async_api_requests
                    .funding_transactions
                    .get(&user_channel_id)
                    .await
                {
                    Some(fee_rate) => fee_rate,
                    None => {
                        error!(
                            "Can't find funding transaction for user_channel_id {user_channel_id}"
                        );
                        return;
                    }
                };
                let funding_tx =
                    match self
                        .wallet
                        .fund_tx(&output_script, &channel_value_satoshis, fee_rate)
                    {
                        Ok(tx) => tx,
                        Err(e) => {
                            error!("Event::FundingGenerationReady: {e}");
                            respond(Err(e));
                            return;
                        }
                    };

                // Give the funding transaction back to LDK for opening the channel.
                if let Err(e) = self
                    .channel_manager
                    .funding_transaction_generated(
                        &temporary_channel_id,
                        &counterparty_node_id,
                        funding_tx.clone(),
                    )
                    .map_err(ldk_error)
                {
                    error!("Event::FundingGenerationReady: {e}");
                    respond(Err(e));
                    return;
                }
                info!("EVENT: Channel with user channel id {user_channel_id} has been funded");
                respond(Ok(funding_tx))
            }
            Event::ChannelPending {
                channel_id,
                user_channel_id,
                former_temporary_channel_id: _,
                counterparty_node_id,
                funding_txo,
            } => {
                info!(
                    "EVENT: Channel {} - {user_channel_id} with counterparty {counterparty_node_id} is pending. OutPoint: {funding_txo}",
                    channel_id.encode_hex::<String>(),
                );
            }
            Event::ChannelReady {
                channel_id,
                user_channel_id,
                counterparty_node_id,
                channel_type: _,
            } => {
                info!(
                    "EVENT: Channel {} - {user_channel_id} with counterparty {counterparty_node_id} is ready to use.",
                    channel_id.encode_hex::<String>(),
                );
            }
            Event::ChannelClosed {
                channel_id,
                reason,
                user_channel_id,
            } => {
                info!(
                    "EVENT: Channel {}: {reason}.",
                    channel_id.encode_hex::<String>()
                );
                self.async_api_requests
                    .funding_transactions
                    .respond(
                        &user_channel_id,
                        Err(anyhow!("Channel closed due to {reason}")),
                    )
                    .await;
            }
            Event::DiscardFunding {
                channel_id,
                transaction,
            } => {
                info!(
                    "EVENT: Funding discarded for channel: {}, txid: {}",
                    channel_id.encode_hex::<String>(),
                    transaction.txid()
                )
            }
            Event::OpenChannelRequest { .. } => {
                // Unreachable, we don't set manually_accept_inbound_channels
            }
            Event::PaymentClaimable {
                payment_hash,
                purpose,
                amount_msat,
                receiver_node_id: _,
                via_channel_id: _,
                via_user_channel_id: _,
                onion_fields: _,
                claim_deadline: _,
            } => {
                info!(
                    "EVENT: received payment from payment hash {} of {} millisatoshis",
                    payment_hash.0.encode_hex::<String>(),
                    amount_msat,
                );
                let payment_preimage = match purpose {
                    PaymentPurpose::InvoicePayment {
                        payment_preimage, ..
                    } => payment_preimage,
                    PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
                };
                if let Some(payment_preimage) = payment_preimage {
                    self.channel_manager.claim_funds(payment_preimage);
                }
            }
            Event::PaymentClaimed {
                payment_hash,
                purpose,
                amount_msat,
                receiver_node_id: _,
            } => {
                info!(
                    "EVENT: claimed payment from payment hash {} of {} millisatoshis",
                    payment_hash.0.encode_hex::<String>(),
                    amount_msat,
                );
                let (payment_preimage, payment_secret) = match purpose {
                    PaymentPurpose::InvoicePayment {
                        payment_preimage,
                        payment_secret,
                        ..
                    } => (payment_preimage, Some(payment_secret)),
                    PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
                };
                let mut payments = self.inbound_payments.lock().unwrap();
                match payments.entry(payment_hash) {
                    Entry::Occupied(mut e) => {
                        let payment = e.get_mut();
                        payment.status = HTLCStatus::Succeeded;
                        payment.preimage = payment_preimage;
                        payment.secret = payment_secret;
                    }
                    Entry::Vacant(e) => {
                        e.insert(PaymentInfo {
                            preimage: payment_preimage,
                            secret: payment_secret,
                            status: HTLCStatus::Succeeded,
                            amt_msat: MillisatAmount(Some(amount_msat)),
                        });
                    }
                }
            }
            Event::PaymentSent {
                payment_preimage,
                payment_hash,
                fee_paid_msat,
                ..
            } => {
                let mut payments = self.outbound_payments.lock().unwrap();
                if let Some(payment) = payments.get_mut(&payment_hash) {
                    payment.preimage = Some(payment_preimage);
                    payment.status = HTLCStatus::Succeeded;
                    info!(
                        "EVENT: successfully sent payment of {} millisatoshis{} from \
								 payment hash {} with preimage {}",
                        payment.amt_msat,
                        if let Some(fee) = fee_paid_msat {
                            format!(" (fee {fee} msat)")
                        } else {
                            "".to_string()
                        },
                        payment_hash.0.encode_hex::<String>(),
                        payment_preimage.0.encode_hex::<String>()
                    );
                }
            }
            Event::PaymentPathSuccessful { .. } => {}
            Event::PaymentPathFailed { .. } => {}
            Event::ProbeSuccessful { .. } => {}
            Event::ProbeFailed { .. } => {}
            Event::PaymentFailed { payment_hash, .. } => {
                info!(
				"EVENT: Failed to send payment to payment hash {}: exhausted payment retry attempts",
				payment_hash.0.encode_hex::<String>()
			);

                let mut payments = self.outbound_payments.lock().unwrap();
                if let Some(payment) = payments.get_mut(&payment_hash) {
                    payment.status = HTLCStatus::Failed;
                }
            }
            Event::PaymentForwarded {
                prev_channel_id,
                next_channel_id,
                fee_earned_msat,
                claim_from_onchain_tx,
                outbound_amount_forwarded_msat,
            } => {
                let read_only_network_graph = self.network_graph.read_only();
                let nodes = read_only_network_graph.nodes();
                let channels = self.channel_manager.list_channels();

                let node_str = |channel_id: &Option<[u8; 32]>| match channel_id {
                    None => String::new(),
                    Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id)
                    {
                        None => String::new(),
                        Some(channel) => {
                            match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
                                None => "private node".to_string(),
                                Some(node) => match &node.announcement_info {
                                    None => "unnamed node".to_string(),
                                    Some(announcement) => {
                                        format!("node {}", announcement.alias)
                                    }
                                },
                            }
                        }
                    },
                };
                let channel_str = |channel_id: &Option<[u8; 32]>| {
                    channel_id
                        .map(|channel_id| {
                            format!(" with channel {}", channel_id.encode_hex::<String>())
                        })
                        .unwrap_or_default()
                };
                let from_prev_str = format!(
                    " from {}{}",
                    node_str(&prev_channel_id),
                    channel_str(&prev_channel_id)
                );
                let to_next_str = format!(
                    " to {}{}",
                    node_str(&next_channel_id),
                    channel_str(&next_channel_id)
                );

                let from_onchain_str = if claim_from_onchain_tx {
                    "from onchain downstream claim"
                } else {
                    "from HTLC fulfill message"
                };
                let amount_str = if let Some(amount) = outbound_amount_forwarded_msat {
                    format!("of amount {amount}")
                } else {
                    "of unknown amount".to_string()
                };
                let fee_str = if let Some(fee_earned) = fee_earned_msat {
                    format!("earning {fee_earned} msat")
                } else {
                    "claimed onchain".to_string()
                };
                info!(
                    "EVENT: Forwarded payment{from_prev_str}{to_next_str} {amount_str}, earning {fee_str} msat {from_onchain_str}",
                );
            }
            Event::HTLCHandlingFailed {
                prev_channel_id,
                failed_next_destination,
            } => {
                error!(
                    "EVENT: Failed handling HTLC from channel {} to {:?}",
                    prev_channel_id.encode_hex::<String>(),
                    failed_next_destination
                );
            }
            Event::PendingHTLCsForwardable { time_forwardable } => {
                let forwarding_channel_manager = self.channel_manager.clone();
                let min = time_forwardable.as_millis() as u64;
                tokio::spawn(async move {
                    let millis_to_sleep = thread_rng().gen_range(min..min * 5);
                    tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
                    forwarding_channel_manager.process_pending_htlc_forwards();
                });
            }
            Event::SpendableOutputs { outputs } => {
                let destination_address = match self.wallet.new_address() {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Could not get new address: {}", e);
                        return;
                    }
                };
                let output_descriptors = &outputs.iter().collect::<Vec<_>>();
                let tx_feerate = self
                    .bitcoind_client
                    .get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
                match self.keys_manager.spend_spendable_outputs(
                    output_descriptors,
                    Vec::new(),
                    destination_address.script_pubkey(),
                    tx_feerate,
                    &Secp256k1::new(),
                ) {
                    Ok(spending_tx) => {
                        info!(
                            "EVENT: Sending spendable output to {}",
                            destination_address.address
                        );
                        self.bitcoind_client.broadcast_transaction(&spending_tx)
                    }
                    Err(_) => {
                        error!("Failed to build spending transaction");
                    }
                };
            }
            Event::HTLCIntercepted {
                intercept_id: _,
                requested_next_hop_scid: _,
                payment_hash: _,
                inbound_amount_msat: _,
                expected_outbound_amount_msat: _,
            } => {}
        }
    }
}
