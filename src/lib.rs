use std::{borrow::Borrow, sync::Arc};

use ethers::{abi::Detokenize, prelude::*};
use tokio::{
    sync::{mpsc, oneshot},
    task,
};

mod dispatcher;

struct TXWithOneshot<B, M, D> {
    // TODO see if we can get away with borrowing function_call
    function_call: FunctionCall<B, M, D>,
    // TODO custom error messages for different kinds of failures
    oneshot_sender: oneshot::Sender<eyre::Result<Option<TransactionReceipt>>>,
}

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
        task::spawn(dispatcher::dispatcher_task(
            provider,
            signing_keys,
            tx_receiver,
        ));
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
