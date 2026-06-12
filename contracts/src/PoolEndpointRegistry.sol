// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Subcall} from "sapphire-contracts/Subcall.sol";

/// @notice Authenticated, on-chain registry of the pool's miner-facing
/// endpoints. Only the genuine ROFL enclave — origin = an authorized instance of
/// `APP_ID`, checked via `Subcall.roflEnsureAuthorizedOrigin` — may write, and it
/// does so through `rofl-appd` app-origin so the funded app account pays gas (no
/// separately-seeded EOA). Anyone may read.
///
/// Purpose: let miners discover the REAL onion address and pin the REAL stratum
/// TLS certificate without trusting DNS, a README, or the rofl.app proxy. The
/// onion (v3) self-authenticates; the TLS fingerprint is the trust anchor for the
/// clearnet/relay path. Both are derived from the enclave's stable KMS identity,
/// so they don't change across redeploys — meaning the enclave only needs to
/// write ONCE (the pool reads first and skips the tx when the stored values
/// already match, so steady-state costs no gas).
contract PoolEndpointRegistry {
    /// ROFL app id (21-byte bech32-decoded `rofl1…`). Immutable — the sole
    /// authority allowed to write. Same model as `FeeSwapper`/`RentPayer`.
    bytes21 public immutable APP_ID;

    /// v3 onion host serving the pool's stratum + read API (e.g.
    /// "abc…xyz.onion"). Empty until first set.
    string public onion;
    /// SHA-256 of the downstream stratum TLS leaf cert (DER). Miners pin this
    /// (`xmrig --tls-fingerprint`). Zero until first set.
    bytes32 public tlsFingerprint;
    /// `block.timestamp` of the last write. 0 = never written.
    uint64 public updatedAt;

    event EndpointsUpdated(string onion, bytes32 tlsFingerprint, uint64 updatedAt);

    constructor(bytes21 appId) {
        APP_ID = appId;
    }

    /// @notice Publish the current endpoints. Callable ONLY by our ROFL enclave;
    /// submit via `rofl-appd` so the app account pays gas. The pool calls this
    /// only when the stored values are missing or differ from what it derives,
    /// so it is not invoked on every boot.
    function setEndpoints(string calldata onion_, bytes32 tlsFingerprint_) external {
        Subcall.roflEnsureAuthorizedOrigin(APP_ID);
        onion = onion_;
        tlsFingerprint = tlsFingerprint_;
        updatedAt = uint64(block.timestamp);
        emit EndpointsUpdated(onion_, tlsFingerprint_, updatedAt);
    }

    /// @notice All endpoints in one call — used by consumers and by the pool's
    /// "write only if changed" pre-check.
    function endpoints()
        external
        view
        returns (string memory onion_, bytes32 tlsFingerprint_, uint64 updatedAt_)
    {
        return (onion, tlsFingerprint, updatedAt);
    }
}
