## Sending Transactions

Now, for the fun part: sending transactions. If you have someone else's testnet address, you can
send them any amount of any asset you have.

First, use balance to find the amount of assets you have:

```bash
cargo run --release --bin pcli view balance
```

Second, if I wanted to send 10 penumbra tokens
to my friend, I could do that like this (filling in their full address at the end):

```bash
cargo run --quiet --release --bin pcli tx send 10penumbra --to penumbrav2t...
```

Notice that asset amounts are typed amounts, specified without a space between the amount (`10`)
and the asset name (`penumbra`). If you have the asset in your wallet to send, then so it shall be done!

## Staking

In addition, to sending an asset, one may also stake penumbra tokens to validators.

Find a validator to stake to:

```bash
cargo run --release --bin pcli query validator list
```

Copy and paste the identity key of one of the validators to stake to, then construct the staking tx:

```bash
cargo run --release --bin pcli tx delegate 10penumbra --to penumbravalid...
```

To undelegate from a validator, use the `pcli tx undelegate` command, passing it the typed amount of
delegation tokens you wish to undelegate. Wait a moment for the network to process the undelegation,
then reclaim your funds:

```bash
cargo run --release --bin pcli tx undelegate-claim
```

Inspect the output; a message may instruct you to wait longer, for a new epoch. Check back and rerun the command
later to add the previously delegated funds to your wallet.

## Governance

Penumbra features on-chain governance similar to Cosmos Hub where anyone can submit proposals and
both validators and delegators to vote on them. Penumbra's governance model incorporates a single
DAO account, into which anyone can freely deposit, but from which only a successful governance vote
can spend. For details on using governance, see the [governance section](./governance.md).

## Managing Liquidity Positions

Penumbra's decentralized exchange ("dex") implementation allows users to create their own on-chain
liquidity positions. The basic structure of a liquidity position expresses a relationship between two
assets, i.e. "I am willing to buy `100penumbra` at a price of `1gm` each, with a fee of `20bps` (base points)" or
"I am willing to sell `100penumbra` at a price of `1gm` each, with a fee of `10bps`".

### Opening a Liquidity Position

The basic commands for opening liquidity positions are `tx position order buy` and `tx position order sell`.

To open an order buying `10cube` at a price of `1penumbra` each, with no fee, you'd do the following:

```bash
cargo run --release --bin pcli -- tx position order buy 10cube@1penumbra --spread 0
```

Similarly, to open an order selling `100penumbra` at a price of `5gm` each, with a `20bps` fee on transactions
against the liquidity position, you'd do the following:

```bash
cargo run --release --bin pcli -- tx position order sell 100penumbra@5gm --spread 20
```

After opening the position, you'll see that your account has been deposited an "LPNFT" representing the open position:

```bash
$ cargo run --release --bin pcli -- view balance

 Account  Amount
 0        1lpnft_opened_plpid1hzrzr2myjw508nf0hyzehl0w0x2xzr4t8vwe6t3qtnfhsqzf5lzsufscqr
```

### Closing a Liquidity Position

If you have an open liquidity position, you may close it, preventing further trading against it.

```bash
cargo run --release --bin pcli -- tx position close plpid1hzrzr2myjw508nf0hyzehl0w0x2xzr4t8vwe6t3qtnfhsqzf5lzsufscqr
```

This will subtract the opened LPNFT and deposit a closed LPNFT into your balance:

```bash
$ cargo run --release --bin pcli -- view balance

 Account  Amount
 0        1lpnft_closed_plpid1hzrzr2myjw508nf0hyzehl0w0x2xzr4t8vwe6t3qtnfhsqzf5lzsufscqr
```

You also have the option to close **all** liquidity positions associated with an address at once. This is useful if you have many individual positions, e.g. due to
trading function approximation:

```bash
cargo run --release --bin pcli -- tx position close-all
```

### Withdrawing a Liquidity Position

If you have a closed liquidity position, you may withdraw it, depositing the reserves in the trading position into your balance.

```bash
cargo run --release --bin pcli -- tx position withdraw plpid1hzrzr2myjw508nf0hyzehl0w0x2xzr4t8vwe6t3qtnfhsqzf5lzsufscqr
```

This will subtract the closed LPNFT and deposit a withdrawn LPNFT into your balance, along with any reserves belonging to the trading position:

```bash
$ cargo run --release --bin pcli -- view balance

 Account  Amount
 0        1lpnft_withdrawn_plpid1hzrzr2myjw508nf0hyzehl0w0x2xzr4t8vwe6t3qtnfhsqzf5lzsufscqr
 0        1cube
```

You also have the option to withdraw **all** liquidity positions associated with an address at once. This is useful if you have many individual positions, e.g. due to
trading function approximation:

```bash
cargo run --release --bin pcli -- tx position withdraw-all
```

## Swapping Assets

One of the most exciting features of Penumbra is that by using IBC (inter-blockchain communication)
and our shielded pool design, **any** tokens can be exchanged in a private way.

Swaps take place against the on-chain liquidity positions described earlier in the guide.

If you wanted to exchange 1 `penumbra` tokens for `gm` tokens, you could do so like so:

```bash
cargo run --release --bin pcli -- tx swap --into gm 1penumbra
```

This will handle generating the swap transaction and you'd soon have the market-rate equivalent of 1 `penumbra`
in `gm` tokens returned to you, or the original investment of 1 `penumbra` tokens returned if there wasn't
enough liquidity available to perform the swap.

## Replicating a UniswapV2 (`x*y=k`) pool 

Penumbra's constant-price pool is a versatile market primitive, allowing users extensive control over their trading strategies. It's not solely for active DEX quoters; with our AMM replication tool, users can emulate any passive AMM of their choice. The testnet comes with a built-in UniswapV2 replicator that is utilized as such:

```bash
cargo run -r --bin pcli tx lp replicate xyk <TRADING_PAIR> <QUANTITY> [--current-price AMT] [--fee-bps AMT]
```
For instance, to provide ~100penumbra and ~100test_usd liquidity on the `penumbra:test_usd` pair with a pool fee of `33bps`, run:

```bash
cargo run -r --bin pcli tx lp replicate xyk penumbra:test_usd 100penumbra --fee-bps 33
```

You will be prompted a disclaimer which you should read carefully, and accept or reject by pressing "y" for yes, or "n" for no.

The replicating market makers tool will then generate a list of positions that you can submit by pressing "y", or reject by pressing "n".

There are other pairs available that you can try this tool on, for example `gm:gn` or `gm:penumbra`.
