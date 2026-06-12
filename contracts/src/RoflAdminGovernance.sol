// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Subcall} from "sapphire-contracts/Subcall.sol";

/// @title RoflAdminGovernance (G1)
/// @notice Minimal on-chain admin for a ROFL app. Set it as the app's on-chain
/// `admin` (`oasis rofl set-admin <thisContract>`). It can ONLY relay a
/// `rofl.Update` to the runtime, and only after a timelock — removing human
/// instant-control of the app while still allowing the one thing G1 needs:
/// rotating the allowed enclave measurements when a forced TDX/TCB update
/// changes them, so the app survives instead of bricking like a hard
/// `admin = null` (G2).
///
/// Deliberately minimal:
/// - It never exposes `rofl.Remove`, so the admin cannot kill the app.
/// - It never builds the update body. The `governor` proposes a full CBOR
///   `rofl.Update` body (produced off-chain by the oasis CLI) which is emitted
///   in the clear, so anyone can decode it during the timelock and verify it
///   only swaps `policy.enclaves` and keeps `admin` = this contract. On-chain
///   enforcement of "enclaves-only" would require CBOR parsing in Solidity; we
///   keep it simple and rely on the timelock + public proposal instead.
///
/// Pool funding is unaffected: rent top-ups are permissionless and never touch
/// the app admin (see deploy/GOVERNANCE.md).
contract RoflAdminGovernance {
    /// Account allowed to propose/cancel/execute. Use a multisig/DAO; keep it
    /// (don't burn) so enclave rotation remains possible across TCB updates.
    address public governor;
    /// Mandatory delay between propose and execute.
    uint256 public immutable delay;
    /// keccak256 of the pending rofl.Update body (0 = none).
    bytes32 public pendingHash;
    /// Earliest execution time of the pending proposal (0 = none).
    uint256 public eta;

    event GovernorTransferred(address indexed from, address indexed to);
    event Proposed(bytes32 indexed bodyHash, bytes body, uint256 eta);
    event Executed(bytes32 indexed bodyHash);
    event Cancelled(bytes32 indexed bodyHash);

    modifier onlyGovernor() {
        require(msg.sender == governor, "not governor");
        _;
    }

    constructor(address _governor, uint256 _delay) {
        require(_governor != address(0), "governor=0");
        governor = _governor;
        delay = _delay;
    }

    function transferGovernor(address to) external onlyGovernor {
        require(to != address(0), "governor=0");
        emit GovernorTransferred(governor, to);
        governor = to;
    }

    /// Stage a full CBOR `rofl.Update` body. Emitted in the clear for public
    /// review during the timelock. Replaces any prior pending proposal.
    function propose(bytes calldata updateBody) external onlyGovernor {
        require(updateBody.length > 0, "empty body");
        pendingHash = keccak256(updateBody);
        eta = block.timestamp + delay;
        emit Proposed(pendingHash, updateBody, eta);
    }

    function cancel() external onlyGovernor {
        emit Cancelled(pendingHash);
        pendingHash = bytes32(0);
        eta = 0;
    }

    /// Relay the timelocked update to the runtime once the delay has elapsed.
    /// `updateBody` must match the staged hash.
    function execute(bytes calldata updateBody) external onlyGovernor {
        require(eta != 0 && block.timestamp >= eta, "timelock");
        require(keccak256(updateBody) == pendingHash, "body mismatch");
        bytes32 h = pendingHash;
        pendingHash = bytes32(0);
        eta = 0;
        (uint64 status, ) = Subcall.subcall("rofl.Update", updateBody);
        require(status == 0, "rofl.Update failed");
        emit Executed(h);
    }
}
