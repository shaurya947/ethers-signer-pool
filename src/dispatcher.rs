use std::{borrow::Borrow, collections::HashMap, sync::Arc};

use ethers::{abi::Detokenize, prelude::*};
use tokio::{
    sync::{mpsc, oneshot},
    task,
};

use super::TXWithOneshot;

mod signer;
use signer::SignerState;

struct TXWithInternalNonce<B, M, D> {
    // TODO see if we can get away with borrowing function_call
    function_call: FunctionCall<B, M, D>,
    internal_nonce: U256,
}

struct BroadcastResponse {
    // TODO custom error messages for different kinds of failures
    broadcast_result: eyre::Result<Option<TransactionReceipt>>,
    internal_nonce: U256,
    new_signer_balance: U256,
    signer_address: H160,
}

pub(super) async fn dispatcher_task<B, M, D, S>(
    provider: Arc<M>,
    signing_keys: Vec<S>,
    mut tx_receiver: mpsc::Receiver<TXWithOneshot<B, M, D>>,
) where
    B: Borrow<M> + Send + Sync + 'static,
    M: Middleware + 'static,
    D: Detokenize + Send + Sync + 'static,
    S: Signer + 'static,
{
    let mut signers = vec![];
    let mut signer_states = HashMap::new();
    for k in signing_keys {
        // TODO error handling
        let balance = provider.get_balance(k.address(), None).await.unwrap();
        signer_states.insert(k.address(), SignerState::Idle { balance });
        signers.push(Arc::new(SignerMiddleware::new(Arc::clone(&provider), k)));
    }

    // TODO think about this bound
    let (bcast_resp_sender, mut bcast_resp_receiver) = mpsc::channel(100);
    let mut internal_nonce = U256::zero();
    let mut oneshot_responder_map = HashMap::new();

    'outer: loop {
        tokio::select! {
            tx_with_oneshot = tx_receiver.recv() => {
                if tx_with_oneshot.is_none() {
                    break 'outer;
                }
                let tx_with_oneshot = tx_with_oneshot.unwrap();
                internal_nonce += U256::one();

                handle_new_tx_with_oneshot(
                    &provider,
                    &signers,
                    &mut signer_states,
                    tx_with_oneshot,
                    internal_nonce,
                    &mut oneshot_responder_map,
                    &bcast_resp_sender,
                )
                .await;
            },
            bcast_resp = bcast_resp_receiver.recv() => {
                let bcast_resp = bcast_resp.unwrap();
                handle_broadcast_response(
                    &mut signer_states,
                    bcast_resp,
                    &mut oneshot_responder_map
                ).await;
            }
        }
    }
}

async fn handle_new_tx_with_oneshot<B, M, D, S>(
    provider: &Arc<M>,
    signers: &[Arc<SignerMiddleware<Arc<M>, S>>],
    signer_states: &mut HashMap<H160, SignerState>,
    tx_with_oneshot: TXWithOneshot<B, M, D>,
    internal_nonce: U256,
    oneshot_responder_map: &mut HashMap<
        U256,
        oneshot::Sender<eyre::Result<Option<TransactionReceipt>>>,
    >,
    bcast_resp_sender: &mpsc::Sender<BroadcastResponse>,
) where
    B: Borrow<M> + Send + Sync + 'static,
    M: Middleware + 'static,
    D: Detokenize + Send + Sync + 'static,
    S: Signer + 'static,
{
    let tx_with_internal_nonce = TXWithInternalNonce {
        function_call: tx_with_oneshot.function_call,
        internal_nonce,
    };
    oneshot_responder_map.insert(internal_nonce, tx_with_oneshot.oneshot_sender);

    // TODO if gas cost estimation fails, just return error in oneshot here
    let estimated_gas_cost =
        estimate_gas_cost(provider.as_ref(), &tx_with_internal_nonce.function_call)
            .await
            .unwrap();
    let good_signer = signer::get_good_signer(signers, signer_states, estimated_gas_cost);

    if let Some(signer) = good_signer {
        let state = signer_states.remove(&signer.address()).unwrap();
        signer_states.insert(
            signer.address(),
            state.add_to_pending_spend(estimated_gas_cost, internal_nonce),
        );

        let signer = Arc::clone(signer);
        let bcast_resp_sender = bcast_resp_sender.clone();
        task::spawn(signer::send_transaction(
            signer,
            bcast_resp_sender,
            tx_with_internal_nonce,
        ));
    } else {
        // TODO return error in oneshot here
        // TODO could consider retrying above two loops 2-3 times before giving up
    }
}

async fn handle_broadcast_response(
    signer_states: &mut HashMap<H160, SignerState>,
    bcast_resp: BroadcastResponse,
    oneshot_responder_map: &mut HashMap<
        U256,
        oneshot::Sender<eyre::Result<Option<TransactionReceipt>>>,
    >,
) {
    let BroadcastResponse {
        signer_address,
        broadcast_result,
        internal_nonce,
        new_signer_balance,
    } = bcast_resp;

    let _ = oneshot_responder_map
        .remove(&internal_nonce)
        .unwrap()
        .send(broadcast_result);

    let state = signer_states.remove(&signer_address).unwrap();
    signer_states.insert(
        signer_address,
        state.remove_from_pending_spend(new_signer_balance, internal_nonce),
    );
}

async fn estimate_gas_cost<B, M, D>(
    provider: &M,
    function_call: &FunctionCall<B, M, D>,
) -> eyre::Result<U256>
where
    B: Borrow<M>,
    M: Middleware + 'static,
    D: Detokenize,
{
    // TODO custom errors + gas scaling (configurable?)
    let gas_price = provider.get_gas_price().await?;
    let gas_units = function_call.estimate_gas().await?;
    Ok(gas_price * gas_units)
}
