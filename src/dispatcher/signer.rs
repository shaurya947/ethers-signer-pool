use std::{borrow::Borrow, collections::HashMap, sync::Arc};

use ethers::{abi::Detokenize, prelude::*};

use tokio::sync::mpsc;

use super::{BroadcastResponse, TXWithInternalNonce};

#[derive(Debug, PartialEq)]
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

#[cfg(test)]
mod signer_state_tests {
    use std::collections::HashMap;

    use ethers::{types::U256, utils::parse_ether};

    use super::SignerState::*;

    #[test]
    fn state_idle_add_to_pending_spend() {
        let state = Idle {
            balance: parse_ether("4.2").unwrap(),
        };

        let state = state.add_to_pending_spend(parse_ether("0.7").unwrap(), U256::from(2));
        assert_eq!(
            state,
            Busy {
                balance: parse_ether("4.2").unwrap(),
                estimated_pending_spend: parse_ether("0.7").unwrap(),
                nonces_and_est_costs: HashMap::from([(U256::from(2), parse_ether("0.7").unwrap())])
            }
        );
    }

    #[test]
    fn state_busy_add_to_pending_spend() {
        let state = Busy {
            balance: parse_ether("4.2").unwrap(),
            estimated_pending_spend: parse_ether("1.5").unwrap(),
            nonces_and_est_costs: HashMap::from([
                (U256::from(2), parse_ether("0.7").unwrap()),
                (U256::from(5), parse_ether("0.8").unwrap()),
            ]),
        };

        let state = state.add_to_pending_spend(parse_ether("0.2").unwrap(), U256::from(6));
        assert_eq!(
            state,
            Busy {
                balance: parse_ether("4.2").unwrap(),
                estimated_pending_spend: parse_ether("1.7").unwrap(),
                nonces_and_est_costs: HashMap::from([
                    (U256::from(2), parse_ether("0.7").unwrap()),
                    (U256::from(5), parse_ether("0.8").unwrap()),
                    (U256::from(6), parse_ether("0.2").unwrap()),
                ]),
            }
        );
    }

    #[test]
    #[should_panic = "Invalid signer state"]
    fn state_idle_remove_from_pending_spend() {
        let state = Idle {
            balance: parse_ether("4.2").unwrap(),
        };

        let _ = state.remove_from_pending_spend(parse_ether("0.7").unwrap(), U256::from(2));
    }

    #[test]
    #[should_panic]
    fn state_busy_remove_from_pending_spend_invalid_nonce() {
        let state = Busy {
            balance: parse_ether("4.2").unwrap(),
            estimated_pending_spend: parse_ether("1.5").unwrap(),
            nonces_and_est_costs: HashMap::from([
                (U256::from(2), parse_ether("0.7").unwrap()),
                (U256::from(5), parse_ether("0.8").unwrap()),
            ]),
        };

        let _ = state.remove_from_pending_spend(parse_ether("3.5").unwrap(), U256::from(6));
    }

    #[test]
    fn state_busy_remove_from_pending_spend_to_idle() {
        let state = Busy {
            balance: parse_ether("4.2").unwrap(),
            estimated_pending_spend: parse_ether("0.7").unwrap(),
            nonces_and_est_costs: HashMap::from([(U256::from(2), parse_ether("0.7").unwrap())]),
        };

        let state = state.remove_from_pending_spend(parse_ether("3.48").unwrap(), U256::from(2));
        assert_eq!(
            state,
            Idle {
                balance: parse_ether("3.48").unwrap()
            }
        );
    }

    #[test]
    fn state_busy_remove_from_pending_spend_still_busy() {
        let state = Busy {
            balance: parse_ether("4.2").unwrap(),
            estimated_pending_spend: parse_ether("1.5").unwrap(),
            nonces_and_est_costs: HashMap::from([
                (U256::from(2), parse_ether("0.7").unwrap()),
                (U256::from(5), parse_ether("0.8").unwrap()),
            ]),
        };

        let state = state.remove_from_pending_spend(parse_ether("3.48").unwrap(), U256::from(2));
        assert_eq!(
            state,
            Busy {
                balance: parse_ether("3.48").unwrap(),
                estimated_pending_spend: parse_ether("0.8").unwrap(),
                nonces_and_est_costs: HashMap::from(
                    [(U256::from(5), parse_ether("0.8").unwrap()),]
                )
            }
        );
    }
}

#[cfg(test)]
mod get_good_signer_tests {
    use std::{collections::HashMap, sync::Arc};

    use ethers::{
        prelude::{rand::thread_rng, SignerMiddleware},
        providers::Provider,
        signers::Wallet,
        types::U256,
        utils::parse_ether,
    };

    use crate::dispatcher::signer::get_good_signer;

    use super::SignerState::*;

    #[test]
    fn good_idle_signer_exists() {
        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let estimated_gas_cost = parse_ether("0.24").unwrap();
        let mut rng = thread_rng();
        let signing_keys = vec![
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
        ];

        // 3 signers
        let signers = signing_keys
            .into_iter()
            .map(|k| Arc::new(SignerMiddleware::new(Arc::clone(&provider), k)))
            .collect::<Vec<_>>();

        // 1st is busy but good, 2nd is idle and good, 3rd is idle and not good
        let signer_states = HashMap::from([
            (
                signers[0].address(),
                Busy {
                    balance: parse_ether("2.8").unwrap(),
                    estimated_pending_spend: parse_ether("0.7").unwrap(),
                    nonces_and_est_costs: HashMap::from([(
                        U256::from(2),
                        parse_ether("0.7").unwrap(),
                    )]),
                },
            ),
            (
                signers[1].address(),
                Idle {
                    balance: parse_ether("4.2").unwrap(),
                },
            ),
            (
                signers[2].address(),
                Idle {
                    balance: parse_ether("0.15").unwrap(),
                },
            ),
        ]);

        assert_eq!(
            get_good_signer(&signers, &signer_states, estimated_gas_cost)
                .unwrap()
                .address(),
            signers[1].address()
        );
    }

    #[test]
    fn good_busy_signer_exists() {
        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let estimated_gas_cost = parse_ether("0.24").unwrap();
        let mut rng = thread_rng();
        let signing_keys = vec![
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
        ];

        // 3 signers
        let signers = signing_keys
            .into_iter()
            .map(|k| Arc::new(SignerMiddleware::new(Arc::clone(&provider), k)))
            .collect::<Vec<_>>();

        // 1st is busy but good, 2nd is idle and not good, 3rd is idle and not good
        let signer_states = HashMap::from([
            (
                signers[0].address(),
                Busy {
                    balance: parse_ether("2.8").unwrap(),
                    estimated_pending_spend: parse_ether("0.7").unwrap(),
                    nonces_and_est_costs: HashMap::from([(
                        U256::from(2),
                        parse_ether("0.7").unwrap(),
                    )]),
                },
            ),
            (
                signers[1].address(),
                Idle {
                    balance: parse_ether("0.2").unwrap(),
                },
            ),
            (
                signers[2].address(),
                Idle {
                    balance: parse_ether("0.15").unwrap(),
                },
            ),
        ]);

        assert_eq!(
            get_good_signer(&signers, &signer_states, estimated_gas_cost)
                .unwrap()
                .address(),
            signers[0].address()
        );
    }

    #[test]
    fn no_good_signer_exists() {
        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let estimated_gas_cost = parse_ether("0.24").unwrap();
        let mut rng = thread_rng();
        let signing_keys = vec![
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
            Wallet::new(&mut rng),
        ];

        // 3 signers
        let signers = signing_keys
            .into_iter()
            .map(|k| Arc::new(SignerMiddleware::new(Arc::clone(&provider), k)))
            .collect::<Vec<_>>();

        // 1st is busy but not good, 2nd is idle and not good, 3rd is idle and not good
        let signer_states = HashMap::from([
            (
                signers[0].address(),
                Busy {
                    balance: parse_ether("0.18").unwrap(),
                    estimated_pending_spend: parse_ether("0.07").unwrap(),
                    nonces_and_est_costs: HashMap::from([(
                        U256::from(2),
                        parse_ether("0.07").unwrap(),
                    )]),
                },
            ),
            (
                signers[1].address(),
                Idle {
                    balance: parse_ether("0.2").unwrap(),
                },
            ),
            (
                signers[2].address(),
                Idle {
                    balance: parse_ether("0.15").unwrap(),
                },
            ),
        ]);

        assert!(get_good_signer(&signers, &signer_states, estimated_gas_cost).is_none());
    }
}
