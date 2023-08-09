use std::{sync::Arc, time::Duration};

use ethers::prelude::{k256::ecdsa::SigningKey, *};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    task,
    time::{self, Instant},
};

pub struct SignerPool {
    tx_sender: mpsc::Sender<TXWithOneshot>,
}

impl SignerPool {
    pub fn new(provider: Arc<Provider<Http>>, signing_keys: Vec<Wallet<SigningKey>>) -> SignerPool {
        // TODO make buffer capacity configurable
        // TODO allow caller to specify optional gas cost estimation function
        let (tx_sender, tx_receiver) = mpsc::channel(100);
        task::spawn(Self::scheduler_task(provider, signing_keys, tx_receiver));
        SignerPool { tx_sender }
    }

    pub async fn send_transactions(
        &self,
        function_calls: Vec<FunctionCall<Arc<Provider<Http>>, Provider<Http>, ()>>,
    ) -> Vec<eyre::Result<TransactionReceipt>> {
        // TODO allow caller to specify optional gas fetching function
        let mut oneshot_receivers = vec![];
        for function_call in function_calls {
            let (oneshot_sender, oneshot_receiver) = oneshot::channel();
            // TODO error handling
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

    async fn scheduler_task(
        provider: Arc<Provider<Http>>,
        signing_keys: Vec<Wallet<SigningKey>>,
        mut tx_receiver: mpsc::Receiver<TXWithOneshot>,
    ) {
        use SignerState::*;

        let mut signer_workers = vec![];
        for k in signing_keys {
            let (tx_sender, tx_receiver) = mpsc::channel(10);
            // TODO error handling
            let signer_current_balance = provider.get_balance(k.address(), None).await.unwrap();
            let signer_state = Arc::new(RwLock::new(Idle {
                balance: signer_current_balance,
            }));
            let signer = Arc::new(SignerMiddleware::new(Arc::clone(&provider), k));
            let signer_worker = SignerWorker {
                signer: Arc::clone(&signer),
                state: Arc::clone(&signer_state),
                tx_sender,
            };
            task::spawn(Self::signer_worker_task(signer, signer_state, tx_receiver));
            signer_workers.push(signer_worker);
        }

        'outer: while let Some(tx_with_oneshot) = tx_receiver.recv().await {
            use SignerState::*;
            let estimated_gas_cost =
                Self::estimate_gas_cost(&provider, &tx_with_oneshot.function_call)
                    .await
                    .unwrap();
            // TODO if gas cost estimation fails, just return error in oneshot here

            // try to find an idle signer that can handle this TX
            for sw in signer_workers.iter() {
                if let Idle { balance } = *sw.state.read().await {
                    if balance >= estimated_gas_cost {
                        {
                            let mut state_write = sw.state.write().await;
                            *state_write = Busy {
                                predicted_next_balance: balance - estimated_gas_cost,
                            }
                        }
                        // TODO handle error
                        let _ = sw.tx_sender.send(tx_with_oneshot).await;
                        continue 'outer;
                    }
                }
            }

            // if no idle signer found, try to find a busy signer
            for sw in signer_workers.iter() {
                if let Busy {
                    predicted_next_balance,
                } = *sw.state.read().await
                {
                    if predicted_next_balance >= estimated_gas_cost {
                        {
                            let mut state_write = sw.state.write().await;
                            *state_write = Busy {
                                predicted_next_balance: predicted_next_balance - estimated_gas_cost,
                            }
                        }
                        // TODO handle error
                        let _ = sw.tx_sender.send(tx_with_oneshot).await;
                        continue 'outer;
                    }
                }
            }

            // TODO if no idle OR busy signer found, return error in oneshot
            // TODO could consider retrying above two loops 2-3 times before giving up
        }
    }

    async fn estimate_gas_cost(
        provider: &Provider<Http>,
        function_call: &FunctionCall<Arc<Provider<Http>>, Provider<Http>, ()>,
    ) -> eyre::Result<U256> {
        // TODO custom errors + gas scaling (configurable?)
        let gas_price = provider.get_gas_price().await?;
        let gas_units = function_call.estimate_gas().await?;
        Ok(gas_price * gas_units)
    }

    async fn signer_worker_task(
        signer: Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
        state: Arc<RwLock<SignerState>>,
        mut tx_receiver: mpsc::Receiver<TXWithOneshot>,
    ) {
        // TODO make balance refresh interval configurable
        let balance_refresh_interval = Duration::from_secs(30);
        let mut balance_refresh_timer = time::interval_at(
            Instant::now() + balance_refresh_interval,
            balance_refresh_interval,
        );

        loop {
            tokio::select! {
                _ = balance_refresh_timer.tick() => {
                    Self::refresh_signer_balance(&signer, &state).await;
                },
                tx_with_oneshot = tx_receiver.recv() => {
                    if let Some(tx_with_oneshot) = tx_with_oneshot {
                        Self::receive_tx_with_oneshot(tx_with_oneshot, &signer, &state).await;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    async fn refresh_signer_balance(
        signer: &Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
        state: &Arc<RwLock<SignerState>>,
    ) {
        use SignerState::*;
        if let Idle { balance: _ } = *state.read().await {
            let mut state_write = state.write().await;
            // TODO error handling
            let new_balance = signer.get_balance(signer.address(), None).await.unwrap();
            *state_write = Idle {
                balance: new_balance,
            };
        }
    }

    async fn receive_tx_with_oneshot(
        tx_with_oneshot: TXWithOneshot,
        signer: &Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
        state: &Arc<RwLock<SignerState>>,
    ) {
        let res = Self::send_transaction(&tx_with_oneshot.function_call, signer).await;
        // TODO is it ok to ignore oneshot send error?
        let _ = tx_with_oneshot.oneshot_sender.send(res);
        Self::reset_signer_state(signer, state).await;
    }

    async fn send_transaction(
        function_call: &FunctionCall<Arc<Provider<Http>>, Provider<Http>, ()>,
        signer: &Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
    ) -> eyre::Result<TransactionReceipt> {
        let mut tx = function_call.tx.clone();
        // TODO custom errors + gas scaling (configurable?)
        tx.set_gas_price(signer.get_gas_price().await?);
        tx.set_gas(function_call.estimate_gas().await?);

        let receipt = signer
            .send_transaction(tx, None)
            .await?
            .await?
            .ok_or(eyre::eyre!("Transaction confirmed but receipt absent"))?;

        Ok(receipt)
    }

    async fn reset_signer_state(
        signer: &Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
        state: &Arc<RwLock<SignerState>>,
    ) {
        use SignerState::*;
        let mut state_write = state.write().await;
        // TODO error handling
        let new_balance = signer.get_balance(signer.address(), None).await.unwrap();
        *state_write = Idle {
            balance: new_balance,
        };
    }
}

struct TXWithOneshot {
    // TODO see if we can get away with borrowing function_call
    function_call: FunctionCall<Arc<Provider<Http>>, Provider<Http>, ()>,
    // TODO custom error messages for different kinds of failures
    oneshot_sender: oneshot::Sender<eyre::Result<TransactionReceipt>>,
}

enum SignerState {
    Idle { balance: U256 },
    Busy { predicted_next_balance: U256 },
}

struct SignerWorker {
    signer: Arc<SignerMiddleware<Arc<Provider<Http>>, Wallet<SigningKey>>>,
    state: Arc<RwLock<SignerState>>,
    tx_sender: mpsc::Sender<TXWithOneshot>,
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
