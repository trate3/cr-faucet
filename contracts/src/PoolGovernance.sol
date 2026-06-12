// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title PoolGovernance
/// @notice Minimal timelocked owner for the pool's `Ownable` contracts
/// (MiningPoolToken, FeeSwapper, RentPayer, PoolEndpointRegistry).
///
/// It removes *instant* human control of the privileged surface: every
/// owner-only call (`setSigner`, `setRedemptionGasSubsidy`, `setOperator`,
/// `setReservoir`, …) must be `queue`d by the governor and
/// can only `execute` after `delay` — a transparent window in which anyone can
/// see the pending change and exit. The governor can `renounce` permanently,
/// after which NOTHING can ever execute again, freezing the owned contracts'
/// admin surface forever (true renouncement) while the contracts keep working.
///
/// Top-ups stay permissionless: rent (`roflmarket.InstanceTopUp` /
/// `RentPayer.receive`) and redemption gas subsidies never route through an
/// owner, so neither the timelock nor renouncement can block funding.
///
/// Ownership is handed over by calling `transferOwnership(thisGovernance)` on
/// each owned contract once setup (e.g. the post-boot `setSigner`/`setOperator`
/// rotation to the KMS address) is complete.
contract PoolGovernance {
    /// Account allowed to queue/execute/cancel. Can be an EOA, a multisig, or a
    /// DAO; set it to a burn address (or `renounce`) for full immutability.
    address public governor;
    /// Mandatory delay between queue and execute.
    uint256 public immutable delay;
    /// Once true, no operation can ever be queued or executed again.
    bool public renounced;
    /// Operation id → earliest execution time (0 = not queued).
    mapping(bytes32 => uint256) public queuedAt;

    event GovernorTransferred(address indexed from, address indexed to);
    event Queued(bytes32 indexed id, address indexed target, uint256 value, bytes data, uint256 eta);
    event Executed(bytes32 indexed id, address indexed target, uint256 value, bytes data);
    event Cancelled(bytes32 indexed id);
    event Renounced();

    modifier onlyGovernor() {
        require(msg.sender == governor, "not governor");
        _;
    }
    modifier live() {
        require(!renounced, "renounced");
        _;
    }

    constructor(address _governor, uint256 _delay) {
        require(_governor != address(0), "governor=0");
        governor = _governor;
        delay = _delay;
    }

    function id(address target, uint256 value, bytes calldata data) public pure returns (bytes32) {
        return keccak256(abi.encode(target, value, data));
    }

    function transferGovernor(address to) external onlyGovernor live {
        require(to != address(0), "governor=0");
        emit GovernorTransferred(governor, to);
        governor = to;
    }

    /// Queue an owner call. Reverts if already queued.
    function queue(address target, uint256 value, bytes calldata data)
        external
        onlyGovernor
        live
        returns (bytes32 opId)
    {
        opId = id(target, value, data);
        require(queuedAt[opId] == 0, "already queued");
        uint256 eta = block.timestamp + delay;
        queuedAt[opId] = eta;
        emit Queued(opId, target, value, data, eta);
    }

    /// Cancel a queued op (allowed even after renounce, to clear state).
    function cancel(address target, uint256 value, bytes calldata data) external onlyGovernor {
        bytes32 opId = id(target, value, data);
        require(queuedAt[opId] != 0, "not queued");
        delete queuedAt[opId];
        emit Cancelled(opId);
    }

    /// Execute a queued op once its timelock has elapsed.
    function execute(address target, uint256 value, bytes calldata data)
        external
        payable
        onlyGovernor
        live
        returns (bytes memory ret)
    {
        bytes32 opId = id(target, value, data);
        uint256 eta = queuedAt[opId];
        require(eta != 0, "not queued");
        require(block.timestamp >= eta, "timelock");
        delete queuedAt[opId];
        bool ok;
        (ok, ret) = target.call{value: value}(data);
        require(ok, "exec failed");
        emit Executed(opId, target, value, data);
    }

    /// Permanently disable governance. The owned contracts keep functioning, but
    /// their owner-only functions become uncallable forever.
    function renounce() external onlyGovernor {
        renounced = true;
        emit Renounced();
    }

    receive() external payable {}
}
