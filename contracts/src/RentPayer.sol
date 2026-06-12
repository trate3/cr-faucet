// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Subcall} from "sapphire-contracts/Subcall.sol";

/// @title RentPayer — autonomous, trustless rent payer for a ROFL marketplace machine
/// @notice The on-chain half of the pool's self-funding loop. The FeeSwapper sends the
/// swapped fee→ROSE here (this contract is the `reservoir`); the enclave then periodically
/// triggers `topUp()` to extend its own machine rental out of THIS contract's balance via
/// `roflmarket.InstanceTopUp` through the Sapphire SUBCALL precompile. No human in the loop.
///
/// Why a contract (not a direct enclave subcall): `rofl-appd`'s `sign-submit` allow-list
/// permits `evm.Call` but NOT `roflmarket.*`, so the enclave calls this contract (an
/// allowed `evm.Call`) and the contract makes the `roflmarket.InstanceTopUp` subcall. The
/// SUBCALL precompile runs the inner call with `caller == this contract`, so the rental
/// `fee` is debited from this contract's native balance (source-verified — see
/// research/rofl-trustless-faucet/05-verified-architecture.md).
///
/// Trust model: `topUp()` is gated to ONLY our enclave via
/// `Subcall.roflEnsureAuthorizedOrigin(APP_ID)` (the origin check inspects the OUTERMOST
/// tx signer and transparently skips intervening `internal` subcall frames). The deployer
/// gets no control lever — the only public entrypoint is `receive()` (anyone may *fund*
/// the rent, nobody can redirect it).
contract RentPayer {
    /// ROFL app id (21-byte bech32-decoded `rofl1…`). Immutable — the app is
    /// sovereign and its id is stable across machine replacement and TCB
    /// changes. This is the sole authority allowed to topUp / retarget.
    bytes21 public immutable APP_ID;
    /// Marketplace provider hosting the CURRENT machine (21-byte bech32-decoded
    /// `oasis1…`). Mutable: the enclave retargets it via `setInstance` when the
    /// app is redeployed to a new machine/provider, so this reservoir contract
    /// (which holds the accumulated ROSE) persists across redeploys.
    bytes21 public provider;
    /// Our CURRENT machine/instance id (8-byte big-endian u64). Mutable — see
    /// `provider`.
    bytes8 public instanceId;

    event ToppedUp(uint8 term, uint8 termCount);
    event Funded(address indexed from, uint256 amount);
    event InstanceUpdated(bytes21 provider, bytes8 instanceId);

    constructor(bytes21 appId, bytes21 provider_, bytes8 instanceId_) {
        APP_ID = appId;
        provider = provider_;
        instanceId = instanceId_;
    }

    /// @notice Retarget to the app's current machine. Callable ONLY by our
    /// enclave (origin = an authorized instance of `APP_ID`). The enclave calls
    /// this on boot with its live provider + instance id, so a redeploy to a new
    /// machine keeps topping up the RIGHT instance without redeploying this
    /// contract or stranding its ROSE balance.
    function setInstance(bytes21 provider_, bytes8 instanceId_) external {
        Subcall.roflEnsureAuthorizedOrigin(APP_ID);
        provider = provider_;
        instanceId = instanceId_;
        emit InstanceUpdated(provider_, instanceId_);
    }

    /// @notice CBOR-encode the `roflmarket.InstanceTopUp` body for this instance.
    /// Exposed (pure-ish `view`, reads immutables) so it can be diffed off-chain against
    /// the oasis-sdk / CLI encoder before trusting the on-chain path, and unit-tested
    /// without the Sapphire SUBCALL precompile. Canonical CBOR, length-first key order
    /// (`id`(2) < `term`(4) < `provider`(8) < `term_count`(10)) — verified byte-for-byte
    /// against `oasis rofl machine top-up --offline --unsigned`.
    /// `term_count` is kept < 24 so the CBOR uint is one minimal byte (a strict decoder
    /// rejects the non-minimal 8-byte form).
    function encodeTopUpBody(uint8 term, uint8 termCount) public view returns (bytes memory) {
        require(term >= 1 && term <= 3, "term must be 1..3");
        require(termCount >= 1 && termCount < 24, "term_count must be 1..23");
        return abi.encodePacked(
            hex"a4", // map(4)
            hex"62", "id", hex"48", instanceId, // "id": bstr(8)
            hex"64", "term", term, // "term": bare uint (1/2/3)
            hex"68", "provider", hex"55", provider, // "provider": bstr(21)
            hex"6a", "term_count", termCount // "term_count": bare uint (<24)
        );
    }

    /// @notice Extend our own machine rental by `termCount` × `term`, paid from this
    /// contract's balance. Callable ONLY by our ROFL enclave (origin = an authorized
    /// instance of `APP_ID`).
    /// @param term      1 = hour, 2 = month, 3 = year
    /// @param termCount number of terms to pre-pay (1..23)
    function topUp(uint8 term, uint8 termCount) external {
        Subcall.roflEnsureAuthorizedOrigin(APP_ID);
        bytes memory body = encodeTopUpBody(term, termCount);
        // Inner call's caller == this contract → rent debited from THIS balance.
        (uint64 status, ) = Subcall.subcall("roflmarket.InstanceTopUp", body);
        require(status == 0, "InstanceTopUp failed");
        emit ToppedUp(term, termCount);
    }

    /// @notice Anyone can fund the rent — the only public lever (fund, not control). The
    /// FeeSwapper sends swapped fee→ROSE here.
    receive() external payable {
        emit Funded(msg.sender, msg.value);
    }
}
