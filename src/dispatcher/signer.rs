use std::{borrow::Borrow, collections::HashMap, sync::Arc};

use ethers::{abi::Detokenize, prelude::*};

use tokio::sync::mpsc;

use super::{BroadcastResponse, TXWithInternalNonce};

pub(super) enum SignerState {
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
    pub(super) fn add_to_pending_spend(
        mut self,
        estimated_gas_cost: U256,
        internal_nonce: U256,
    ) -> Self {
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

    pub(super) fn remove_from_pending_spend(
        mut self,
        new_balance: U256,
        internal_nonce: U256,
    ) -> Self {
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

pub(super) fn get_good_signer<'a, M, S>(
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

pub(super) async fn send_transaction<B, M, D, S>(
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
