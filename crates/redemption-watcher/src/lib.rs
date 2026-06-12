//! Redemption pipeline:
//!  - `events`   — HTTP `eth_getLogs` poller. Reads MiningPoolToken.Redemption,
//!                 writes to `redemptions:queue`.
//!  - `payouts`  — STREAM consumer. Reads the queue, calls monero-wallet-rpc
//!                 to pay out.
//!  - `treasury` — periodic refresher that snapshots wallet balance +
//!                 pending obligations for the operator-api to serve.

pub mod events;
pub mod marker;
pub mod payouts;
pub mod treasury;
pub mod fee_swap;
