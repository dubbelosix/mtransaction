use crate::metrics;
use log::{debug, error, info};
use solana_client::{
    nonblocking::pubsub_client::PubsubClient, rpc_client::RpcClient,
    rpc_response::RpcVoteAccountStatus,
};
use solana_sdk::{
    commitment_config::CommitmentConfig, native_token::LAMPORTS_PER_SOL, signature::Signature,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    str::FromStr,
    sync::Arc,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};

pub fn solana_client(url: String, commitment: String) -> RpcClient {
    RpcClient::new_with_commitment(url, CommitmentConfig::from_str(&commitment).unwrap())
}

pub fn get_activated_stake(
    client: &RpcClient,
) -> Result<HashMap<String, u64>, Box<dyn Error + Send + Sync>> {
    let RpcVoteAccountStatus {
        current,
        delinquent: _,
    } = client.get_vote_accounts()?;

    Ok(current
        .iter()
        .map(|account| {
            (
                account.node_pubkey.clone(),
                account.activated_stake / LAMPORTS_PER_SOL,
            )
        })
        .collect())
}

pub fn get_current_epoch(client: &RpcClient) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let epoch_info = client.get_epoch_info()?;

    Ok(epoch_info.epoch)
}

pub fn slot_stream(pubsub_client: Arc<PubsubClient>) -> Result<(), Box<dyn Error + Send + Sync>> {
    tokio::spawn(async move {
        let (mut slot_notifications, _slot_unsubscribe) = pubsub_client.slot_subscribe().await?;

        while let Some(slot_info) = slot_notifications.next().await {
            info!("slot: {:?}", slot_info);
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    });
    Ok(())
}

pub fn get_leader_schedule(
    client: &RpcClient,
) -> Result<HashMap<u64, String>, Box<dyn Error + Send + Sync>> {
    let leader_schedule = client
        .get_leader_schedule(None)?
        .expect("No leader schedule!");

    Ok(leader_schedule
        .iter()
        .map(|(identity, slots)| slots.iter().map(|slot| (*slot as u64, identity.clone())))
        .flatten()
        .collect())
}

pub fn get_tpu_by_identity(
    client: &RpcClient,
) -> Result<HashMap<String, String>, Box<dyn Error + Send + Sync>> {
    let nodes = client.get_cluster_nodes()?;

    Ok(nodes
        .iter()
        .flat_map(|node| match node.tpu {
            Some(tpu) => Some((node.pubkey.clone(), tpu.to_string())),
            _ => None,
        })
        .collect())
}

const MAX_LEADERS: u64 = 5;
pub fn leaders_stream(
    client: Arc<RpcClient>,
    pubsub_client: Arc<PubsubClient>,
) -> Result<UnboundedReceiver<HashSet<String>>, Box<dyn Error + Send + Sync>> {
    let (tx, rx) = unbounded_channel();

    tokio::spawn(async move {
        let mut refresh_leaders_schedule_hint = Box::pin(
            tokio_stream::iter(std::iter::repeat(()))
                .throttle(tokio::time::Duration::from_secs(3600)),
        ); // todo implement some sound logic to refresh
        let (mut slot_notifications, _slot_unsubscribe) = pubsub_client.slot_subscribe().await?;

        let mut schedule = Default::default();
        let mut last_leaders: HashSet<String> = Default::default();

        loop {
            tokio::select! {
                _ = refresh_leaders_schedule_hint.next() => {
                    info!("Will refresh leaders..");
                    schedule = get_leader_schedule(client.as_ref())?;
                },
                Some(slot_info) = slot_notifications.next() => {
                    let current_leaders: HashSet<_> = (0..MAX_LEADERS)
                        .map(|nth_leader| nth_leader * 4 + (slot_info.slot % 432000))
                        .map(|slot| schedule.get(&slot))
                        .flatten()
                        .cloned()
                        .collect();
                    debug!("Slot: {:?}, {:?}", slot_info, &current_leaders);
                    if !current_leaders.eq(&last_leaders) {
                        if let Err(err) = tx.send(current_leaders.clone()) {
                            error!("Failed to propagate new leaders: {}", err);
                        }
                        last_leaders = current_leaders;
                    }
                },
                else => break,
            }
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    });

    Ok(rx)
}

struct SignatureRecord {
    created_at: tokio::time::Instant,
    signature: Signature,
}
pub fn spawn_tx_signature_watcher(
    client: Arc<RpcClient>,
) -> Result<UnboundedSender<Signature>, Box<dyn Error + Send + Sync>> {
    let (tx_signature, rx_signature) = unbounded_channel::<Signature>();

    let mut rx_signature = UnboundedReceiverStream::new(rx_signature);

    let mut bundle_subscriptions_signal = Box::pin(
        tokio_stream::iter(std::iter::repeat(())).throttle(tokio::time::Duration::from_secs(1)),
    );

    let signature_check_after = tokio::time::Duration::from_secs(10);
    let max_bundle_size = 250;

    tokio::spawn(async move {
        let mut signature_queue: VecDeque<SignatureRecord> = Default::default();

        loop {
            tokio::select! {
                _ = bundle_subscriptions_signal.next() => {
                    loop {
                        let mut to_be_bundled_count = 0;
                        for record in signature_queue.iter() {
                            if record.created_at.elapsed() > signature_check_after && to_be_bundled_count < max_bundle_size {
                                to_be_bundled_count += 1;
                            } else {
                                break;
                            }
                        }
                        if to_be_bundled_count == 0 {
                            break;
                        }
                        {
                            let bundle = signature_queue.drain(0..to_be_bundled_count).map(|r| r.signature).collect::<Vec<_>>();

                            spawn_signature_checker(client.clone(), bundle);
                        }
                    }
                },
                Some(signature) = rx_signature.next() => {
                    signature_queue.push_back(SignatureRecord {
                        created_at: tokio::time::Instant::now(),
                        signature: signature.clone(),
                    });
                    info!("Will watch for {:?}", &signature);
                },
                else => break,
            }
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    });

    Ok(tx_signature)
}

fn spawn_signature_checker(client: Arc<RpcClient>, bundle: Vec<Signature>) {
    tokio::spawn(async move {
        match client.get_signature_statuses(&bundle) {
            Ok(response) => {
                for signature_status in response.value {
                    if let Some(known_status) = signature_status {
                        info!("Signature status {:?}", known_status);
                        match known_status.err {
                            Some(_) => metrics::CHAIN_TX_EXECUTION_SUCCESS.inc(),
                            _ => metrics::CHAIN_TX_EXECUTION_SUCCESS.inc(),
                        };
                        metrics::CHAIN_TX_FINALIZED.inc();
                    } else {
                        metrics::CHAIN_TX_TIMEOUT.inc();
                    }
                }
            }
            Err(err) => {
                error!("Failed to get signature statuses: {}", err);
                metrics::CHAIN_TX_TIMEOUT.inc_by(bundle.len() as u64);
            }
        }
    });
}
