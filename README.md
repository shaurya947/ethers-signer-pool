# ethers-signer-pool
This library provides a utility built on top of the [ethers](https://github.com/gakonst/ethers-rs) crate. The purpose of the utility is to provide a pool of signers (wallets) that can process an arbitrary number of transactions concurrently. As such, a user of this library only needs to instantiate a `SignerPool` once with a middleware (provider), as well as a list of signing keys. From that point on, the user can simply interface with the library by passing in a list of function calls that encapsulate the transactions that the user would like executed, and the signer pool will _schedule_ each transaction on an appropriate signer. Transaction broadcast and confirmation is done in a new task so as to introduce concurrency where it matters most.

TODO: add example code with usage

## High-level architecture

The `SignerPool` can be broken down into 4 parts: the API, the dispatcher task, the signers, and the futures/tasks used for broadcasting and confirmations.

![image](https://github.com/shaurya947/ethers-signer-pool/assets/3454081/66870de8-729b-4f08-938c-bdb07f65f5c2)

- **User** sends list of function calls to **API** and expects a list of results wrapped inside a future.
- **API** receives a list of function calls from the **User** and maps each one into a new compound object `TXWithOneshot` which contains a oneshot sender in addition to the function call. **API** sends `TXWithOneshot` objects to the **dispatcher** via a channel.
- **API** awaits messages on the list of oneshot receivers and returns a list of the received results back to the **user**.
- **Dispatcher** receives `TXWithOneshot` instances from the **API** in a channel. It manages an internal nonce of its own. It assings each incoming function call a nonce, and stores the oneshot sender in a [nonce=>oneshot sender] mapping. It then calculates the estimated gas cost of the transaction, and packages the function call + nonce into a new compound object `TXWithInternalNonce` and looks for an appropriate `signer` to handle the transaction. Idle signers are checked first. Then busy signers are checked.
  - If an idle signer is picked, its state is changed to busy with a _pending spent amount_ equal to the estimated gas cost of the transaction, and an object with the TX's estimated cost + internal nonce is added to the signer's active set.
  - If a busy signer is picked, its pending spend amount and active set are added to accordingly.
- Once a signer is picked, a new task is spawned for broadcasting the TX and waiting for its confirmation. This task is given a clone of a sender end of a channel **that is shared between all TX tasks and the dispatcher**. This tasks returns the result of the broadcast (along with some other identifying information about the signer and the broadcasted transaction) back to the dispatcher using the provided sender.
- The **dispatcher** is listening for TX results at the receiving end of the channel whose senders are given to the TX broadcast tasks. When it receives a message on this channel it does two things:
  - It updates the particular signer's balance and updates its state (makes it _less busy_ or idle)
  - It finds the oneshot sender corresponding to the particular TX and sends the result/error down

