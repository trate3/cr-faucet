// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Subcall} from "sapphire-contracts/Subcall.sol";

/// @title RentPayer — self-funding + trustless rent payer for a ROFL marketplace machine
/// @notice The single on-chain piece that satisfies BOTH side quests:
///   1. Self-funding: holds TEST and pays for the machine's rental out of its own balance
///      via `roflmarket.InstanceTopUp` through the Sapphire SUBCALL precompile. The ROFL
///      enclave triggers `topUp()` itself (appd `evm.Call`), so no human tops it up.
///   2. Trustless: `topUp()` is gated to ONLY our enclave via Subcall.roflEnsureAuthorizedOrigin.
///      The deployer gets no control lever — only a *fund* lever (anyone may send TEST here).
///
/// All encodings verified against the oasis CLI's own InstanceTopUp encoder — see
/// research/rofl-trustless-faucet/05-verified-architecture.md.
contract RentPayer {
    bytes21 public immutable APP_ID;      // ROFL app id (21-byte bech32-decoded rofl1..)
    bytes21 public immutable PROVIDER;    // marketplace provider (21-byte bech32-decoded oasis1..)
    bytes8 public immutable INSTANCE_ID;  // our machine/instance id (8-byte big-endian u64)

    event ToppedUp(uint8 term, uint8 termCount);
    event Funded(address indexed from, uint256 amount);

    constructor(bytes21 appId, bytes21 provider, bytes8 instanceId) {
        APP_ID = appId;
        PROVIDER = provider;
        INSTANCE_ID = instanceId;
    }

    /// @notice Extend our own rental. Callable ONLY by our ROFL enclave (the outermost tx
    ///         signer must be an authorized instance of APP_ID; intervening subcall frames
    ///         are `internal` and transparently skipped by the origin check).
    /// @param term       1 = hour, 2 = month, 3 = year
    /// @param termCount  number of terms to pre-pay (1..23 so the CBOR uint stays one byte)
    function topUp(uint8 term, uint8 termCount) external {
        require(term >= 1 && term <= 3, "term must be 1..3");
        require(termCount >= 1 && termCount < 24, "term_count must be 1..23");

        Subcall.roflEnsureAuthorizedOrigin(APP_ID);

        // CBOR(InstanceTopUp{ id, term, provider, term_count }) — canonical length-first
        // key order, verified byte-for-byte against `oasis rofl machine top-up --unsigned`.
        bytes memory body = abi.encodePacked(
            hex"a4",
            hex"62", "id",         hex"48", INSTANCE_ID,   // bstr(8)
            hex"64", "term",       term,                   // bare uint (1/2/3)
            hex"68", "provider",   hex"55", PROVIDER,      // bstr(21)
            hex"6a", "term_count", termCount               // bare uint (<24)
        );

        // Inner call's caller == this contract → rent paid from THIS contract's balance.
        (uint64 status, ) = Subcall.subcall("roflmarket.InstanceTopUp", body);
        require(status == 0, "InstanceTopUp failed");

        emit ToppedUp(term, termCount);
    }

    /// @notice Anyone can fund the rent. The only public lever — fund, not control.
    receive() external payable {
        emit Funded(msg.sender, msg.value);
    }
}
