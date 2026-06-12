// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {ERC20Burnable} from "@openzeppelin/contracts/token/ERC20/extensions/ERC20Burnable.sol";
import {ERC20Permit} from "@openzeppelin/contracts/token/ERC20/extensions/ERC20Permit.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {IUniswapV2Factory, IUniswapV2Router02} from "./interfaces/IUniswapV2.sol";

/// @title MiningPoolToken
/// @notice The mining pool's own ERC-20 — distinct from the Crossroads
/// asset. Represents a claim on the pool's accumulated XMR (redeemable for
/// XMR via the pool's payout pipeline), minted via cumulative vouchers signed
/// by the pool's enclave key.
/// `claimed[user]` stores lifetime cumulative minted; vouchers carry the
/// monotonically increasing total balance owed, so each successive claim mints
/// only the delta. This single mechanism prevents double-mint AND supports
/// unlimited repeated claims as the miner accumulates more shares.
contract MiningPoolToken is ERC20Burnable, ERC20Permit, Ownable {
    using ECDSA for bytes32;

    bytes32 private constant VOUCHER_TYPEHASH = keccak256(
        "Voucher(address user,uint256 cumulativeAmount,uint256 signedAt)"
    );

    address public authorizedSigner;
    mapping(address => uint256) public claimed;

    event SignerUpdated(address indexed signer);
    event Claimed(address indexed user, uint256 delta, uint256 newCumulative);
    event Redemption(address indexed user, uint256 amount, string xmrAddress, uint256 id);
    event RedemptionProcessed(uint256 indexed id, string moneroTxid);
    event RedemptionGasSubsidyUpdated(uint256 newSubsidy);
    event RestoreHeightUpdated(uint256 height);

    /// Stored redemption requests, keyed by 1-based id. The off-chain
    /// pool reads `nextRedemptionId` and pages through `redemptions(i)`
    /// for any ids past its cursor; we deliberately do NOT rely on
    /// `eth_getLogs` since Sapphire's per-call block-range cap (~100)
    /// makes catch-up slow and brittle.
    struct StoredRedemption {
        address user;
        uint256 amount;
        string xmrAddress;
    }
    mapping(uint256 => StoredRedemption) public redemptions;
    /// Id of the most recently issued redemption; 0 means none yet.
    uint256 public nextRedemptionId;

    /// Durable record that the TEE pool has paid out a redemption. The
    /// pool's processing state otherwise lives only in its (wipe-prone,
    /// single-machine) ROFL disk; if that's lost on a provider switch the
    /// id-poller would re-enqueue and DOUBLE-PAY already-settled
    /// redemptions. The pool marks each one processed on-chain the moment
    /// the Monero withdraw hits the mempool, and on boot skips any id
    /// already flagged here. Only the enclave (`authorizedSigner`) can
    /// set it.
    mapping(uint256 => bool) public processed;
    /// Monero txid of the payout, for auditability. Set alongside
    /// `processed[id]`.
    mapping(uint256 => string) public payoutTxid;

    /// PERFORMANCE-ONLY hint: the Monero block height a from-seed wallet restore
    /// can start scanning from (the oldest still-unspent output the pool holds).
    /// Monotonic; advanced by the enclave inside `markProcessed` so a wiped
    /// instance restores fast instead of rescanning from wallet birth. It has NO
    /// bearing on authorization or the double-pay guard (that's the processed
    /// markers + the payout's stamped amount).
    uint256 public restoreHeight;

    /// Minimum native value a `redeem()` MUST attach. It is forwarded to
    /// `authorizedSigner` (the enclave's L2 account) to pre-fund that
    /// redemption's own on-chain `markProcessed` tx. Enforcing it makes the
    /// durable payout record self-funding: a redemption cannot be created
    /// unless it carries the gas to record itself as processed, closing the
    /// gap where a broke signer silently skips the mark. Owner-tunable as L2
    /// gas prices move; set it to a comfortable multiple of `markProcessed`'s
    /// gas cost at the prevailing gas price.
    uint256 public redemptionGasSubsidy;

    /// The MPT/wrapped-native UniswapV2 pair created at deploy. The pool's
    /// FeeSwapper sells fee-MPT into this pair for ROSE to fund rent. Zero if
    /// no factory was supplied (e.g. minimal/test deploys).
    address public immutable pair;

    /// @param signer_ enclave EOA authorized to sign vouchers / mark redemptions.
    /// @param redemptionGasSubsidy_ mandatory native value attached to redeem().
    /// @param uniswapRouter UniswapV2 router; if non-zero, an `MPT/WETH` pair is
    ///        created on deploy (using the router's own factory + WETH, so the
    ///        FeeSwapper's swaps route through the same pair) — a ROSE market
    ///        exists from block 0. Pass address(0) to skip pool creation
    ///        (minimal/test deploys).
    constructor(
        address signer_,
        uint256 redemptionGasSubsidy_,
        address uniswapRouter
    )
        ERC20("MiningPoolToken", "MPT")
        ERC20Permit("MiningPoolToken")
        Ownable(msg.sender)
    {
        authorizedSigner = signer_;
        redemptionGasSubsidy = redemptionGasSubsidy_;
        emit SignerUpdated(signer_);
        emit RedemptionGasSubsidyUpdated(redemptionGasSubsidy_);

        if (uniswapRouter != address(0)) {
            IUniswapV2Router02 router = IUniswapV2Router02(uniswapRouter);
            address factory = router.factory();
            address weth = router.WETH();
            address existing = IUniswapV2Factory(factory).getPair(address(this), weth);
            pair = existing != address(0)
                ? existing
                : IUniswapV2Factory(factory).createPair(address(this), weth);
        }
    }

    function decimals() public pure override returns (uint8) {
        return 12; // matches XMR atomic units
    }

    /// Tune the mandatory redeem gas subsidy as L2 gas prices move.
    function setRedemptionGasSubsidy(uint256 newSubsidy) external onlyOwner {
        redemptionGasSubsidy = newSubsidy;
        emit RedemptionGasSubsidyUpdated(newSubsidy);
    }

    function setSigner(address signer_) external onlyOwner {
        authorizedSigner = signer_;
        emit SignerUpdated(signer_);
    }

    /// @notice Mint up to `cumulativeAmount` total to `user`. Anyone may call;
    /// the voucher binds `user`, so a relayer cannot redirect tokens.
    /// @param signedAt Unix time the voucher was signed. Recorded in the
    /// signature (so it can serve as a freshness/ordering signal off-chain) but
    /// intentionally NOT enforced on-chain: double-mint is already prevented
    /// structurally by the `cumulativeAmount > claimed[user]` watermark, so no
    /// validity window is needed here. A window can be re-added later by
    /// requiring `block.timestamp <= signedAt + WINDOW`.
    function claim(
        address user,
        uint256 cumulativeAmount,
        uint256 signedAt,
        bytes calldata sig
    ) external returns (uint256 delta) {
        require(cumulativeAmount > claimed[user], "no new balance");

        bytes32 structHash = keccak256(
            abi.encode(VOUCHER_TYPEHASH, user, cumulativeAmount, signedAt)
        );
        bytes32 digest = _hashTypedDataV4(structHash);
        address recovered = digest.recover(sig);
        require(recovered == authorizedSigner, "bad voucher sig");

        delta = cumulativeAmount - claimed[user];
        claimed[user] = cumulativeAmount;
        _mint(user, delta);
        emit Claimed(user, delta, cumulativeAmount);
    }

    /// @notice Burn `amount` and request `amount` atomic XMR be sent to
    /// `xmrAddress`. Picked up by the off-chain redemption-watcher.
    ///
    /// @dev `payable` and the attached value is MANDATORY: the caller must
    /// attach at least `redemptionGasSubsidy` of native gas token (ROSE),
    /// which is forwarded straight to `authorizedSigner` (the enclave's
    /// account) to fund this redemption's own `markProcessed` transaction.
    /// The pool has no other L2 income and its KMS-derived account starts
    /// empty, so without this a payout could be made with no durable on-chain
    /// record — risking a double-pay after a disk wipe. Requiring the subsidy
    /// makes the processed-marker self-funding: no gas, no redemption.
    function redeem(uint256 amount, string calldata xmrAddress) external payable returns (uint256 id) {
        require(msg.value >= redemptionGasSubsidy, "redeem: gas subsidy required");

        _burn(msg.sender, amount);
        unchecked {
            nextRedemptionId += 1;
        }
        id = nextRedemptionId;
        redemptions[id] = StoredRedemption({
            user: msg.sender,
            amount: amount,
            xmrAddress: xmrAddress
        });
        emit Redemption(msg.sender, amount, xmrAddress, id);

        // Forward the subsidy to the enclave account. Required to succeed: the
        // entire point is to fund the signer, and `authorizedSigner` is the
        // enclave's EOA (owner-set), for which a plain value-call never fails.
        if (msg.value > 0) {
            (bool ok, ) = payable(authorizedSigner).call{value: msg.value}("");
            require(ok, "redeem: subsidy transfer failed");
        }
    }

    /// @notice Record that redemption `id` has been paid out, with its
    /// Monero `moneroTxid`. Only the enclave may call this. Idempotent:
    /// re-marking a processed id is a no-op (keeps the first txid).
    ///
    /// The watcher calls this right after broadcasting the XMR withdraw,
    /// so the durable record exists even if the enclave's disk is wiped a
    /// moment later. On boot the watcher reads `processed[id]` and skips
    /// anything already settled — the anti-double-pay guarantee.
    /// @param newRestoreHeight if greater than the current `restoreHeight`,
    /// advance it in this same tx (one tx marks the payout AND moves the restore
    /// pointer forward). Pass 0 to mark only. Performance hint only.
    function markProcessed(uint256 id, string calldata moneroTxid, uint256 newRestoreHeight) external {
        require(msg.sender == authorizedSigner, "not authorized");
        require(id != 0 && id <= nextRedemptionId, "unknown redemption");
        // Advance the restore pointer regardless of the idempotent early-return
        // below, so it can ride along even on a re-mark.
        if (newRestoreHeight > restoreHeight) {
            restoreHeight = newRestoreHeight;
            emit RestoreHeightUpdated(newRestoreHeight);
        }
        if (processed[id]) {
            return;
        }
        processed[id] = true;
        payoutTxid[id] = moneroTxid;
        emit RedemptionProcessed(id, moneroTxid);
    }
}
