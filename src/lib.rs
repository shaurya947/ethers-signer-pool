use std::{borrow::Borrow, collections::HashMap, panic, sync::Arc};

use ethers::{abi::Detokenize, prelude::*};
use tokio::{
    sync::{mpsc, oneshot},
    task,
};

pub struct SignerPool<B, M, D> {
    tx_sender: mpsc::Sender<TXWithOneshot<B, M, D>>,
}

impl<B, M, D> SignerPool<B, M, D>
where
    B: Borrow<M> + Send + Sync + 'static,
    M: Middleware + 'static,
    D: Detokenize + Send + Sync + 'static,
{
    pub fn new<S>(provider: Arc<M>, signing_keys: Vec<S>) -> SignerPool<B, M, D>
    where
        S: Signer + 'static,
    {
        // TODO make buffer capacity configurable
        let (tx_sender, tx_receiver) = mpsc::channel(100);
        task::spawn(dispatcher_task(provider, signing_keys, tx_receiver));
        SignerPool { tx_sender }
    }

    pub async fn send_transactions(
        &self,
        function_calls: Vec<FunctionCall<B, M, D>>,
    ) -> Vec<eyre::Result<Option<TransactionReceipt>>> {
        // TODO allow caller to specify optional gas fetching function
        // TODO allow caller to specify optional gas cost estimation function
        let mut oneshot_receivers = vec![];
        for function_call in function_calls {
            let (oneshot_sender, oneshot_receiver) = oneshot::channel();
            // TODO error handling
            // TODO could consider sending full vec as batch
            self.tx_sender
                .send(TXWithOneshot {
                    function_call,
                    oneshot_sender,
                })
                .await
                .unwrap();
            oneshot_receivers.push(oneshot_receiver);
        }

        // TODO could consider returning a vec of futures and let user manage await order
        // TODO could consider a timeout
        let mut results = vec![];
        for receiver in oneshot_receivers {
            results.push(receiver.await.unwrap()); // TODO error handling
        }
        results
    }
}

async fn dispatcher_task<B, M, D, S>(
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
    let good_signer = get_good_signer(signers, signer_states, estimated_gas_cost);

    if let Some(signer) = good_signer {
        let state = signer_states.remove(&signer.address()).unwrap();
        signer_states.insert(
            signer.address(),
            state.add_to_pending_spend(estimated_gas_cost, internal_nonce),
        );

        let signer = Arc::clone(signer);
        let bcast_resp_sender = bcast_resp_sender.clone();
        task::spawn(send_transaction(
            signer,
            bcast_resp_sender,
            tx_with_internal_nonce,
        ));
    } else {
        // TODO return error in oneshot here
        // TODO could consider retrying above two loops 2-3 times before giving up
    }
}

fn get_good_signer<'a, M, S>(
    signers: &'a [Arc<SignerMiddleware<Arc<M>, S>>],
    signer_states: &HashMap<H160, SignerState>,
    estimated_gas_cost: U256,
) -> Option<&'a Arc<SignerMiddleware<Arc<M>, S>>>
where
    M: Middleware,
    S: Signer,
{
    use SignerState::*;

    // try idle signers first
    let mut good_signer = signers.iter().find(|s| {
        if let Some(Idle { balance }) = signer_states.get(&s.address()) {
            *balance >= estimated_gas_cost
        } else {
            false
        }
    });

    // if no idle signer worked, try busy signers
    if good_signer.is_none() {
        good_signer = signers.iter().find(|s| {
            if let Some(Busy {
                balance,
                estimated_pending_spend,
                ..
            }) = signer_states.get(&s.address())
            {
                *balance - *estimated_pending_spend >= estimated_gas_cost
            } else {
                false
            }
        });
    }

    good_signer
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

async fn send_transaction<B, M, D, S>(
    signer: Arc<SignerMiddleware<Arc<M>, S>>,
    bcast_resp_sender: mpsc::Sender<BroadcastResponse>,
    tx_with_internal_nonce: TXWithInternalNonce<B, M, D>,
) where
    B: Borrow<M>,
    M: Middleware,
    D: Detokenize,
    S: Signer,
{
    let mut tx = tx_with_internal_nonce.function_call.tx.clone();
    // TODO custom errors + gas scaling (configurable?)
    tx.set_gas_price(signer.get_gas_price().await.unwrap());
    tx.set_gas(
        tx_with_internal_nonce
            .function_call
            .estimate_gas()
            .await
            .unwrap(),
    );

    // TODO custom errors
    let broadcast_result = signer
        .send_transaction(tx, None)
        .await
        .unwrap()
        .await
        .map_err(|_| eyre::eyre!("Oops")); // TODO find way to remove this

    // TODO ok to ignore?
    let _ = bcast_resp_sender
        .send(BroadcastResponse {
            broadcast_result,
            internal_nonce: tx_with_internal_nonce.internal_nonce,
            // TODO error handling here (or ignore with option?)
            new_signer_balance: signer.get_balance(signer.address(), None).await.unwrap(),
            signer_address: signer.address(),
        })
        .await;
}

struct TXWithOneshot<B, M, D> {
    // TODO see if we can get away with borrowing function_call
    function_call: FunctionCall<B, M, D>,
    // TODO custom error messages for different kinds of failures
    oneshot_sender: oneshot::Sender<eyre::Result<Option<TransactionReceipt>>>,
}

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

enum SignerState {
    Idle {
        balance: U256,
    },
    Busy {
        balance: U256,
        estimated_pending_spend: U256,
        nonces_and_est_costs: HashMap<U256, U256>,
    },
}

impl SignerState {
    fn add_to_pending_spend(mut self, estimated_gas_cost: U256, internal_nonce: U256) -> Self {
        use SignerState::*;

        match &mut self {
            Idle { balance } => Busy {
                balance: *balance,
                estimated_pending_spend: estimated_gas_cost,
                nonces_and_est_costs: HashMap::from([(internal_nonce, estimated_gas_cost)]),
            },
            Busy {
                estimated_pending_spend,
                nonces_and_est_costs,
                ..
            } => {
                *estimated_pending_spend += estimated_gas_cost;
                nonces_and_est_costs.insert(internal_nonce, estimated_gas_cost);
                self
            }
        }
    }

    fn remove_from_pending_spend(mut self, new_balance: U256, internal_nonce: U256) -> Self {
        use SignerState::*;

        match &mut self {
            Idle { .. } => panic!("Invalid signer state"),
            Busy {
                balance,
                estimated_pending_spend,
                nonces_and_est_costs,
            } => {
                *estimated_pending_spend -= nonces_and_est_costs.remove(&internal_nonce).unwrap();
                *balance = new_balance;

                if nonces_and_est_costs.is_empty() {
                    Idle {
                        balance: new_balance,
                    }
                } else {
                    self
                }
            }
        }
    }
}

// pub fn add(left: usize, right: usize) -> usize {
//     left + right
// }

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn it_works() {
//         let result = add(2, 2);
//         assert_eq!(result, 4);
//     }
// }
