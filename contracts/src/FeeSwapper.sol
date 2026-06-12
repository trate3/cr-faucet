// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Subcall} from "sapphire-contracts/Subcall.sol";
import {IUniswapV2Router02} from "./interfaces/IUniswapV2.sol";

/// @notice Sells fee-MPT for native ROSE on a UniswapV2 pool and forwards the
/// proceeds to the rent reservoir.
///
/// Fee-MPT is minted to THIS contract through the pool's normal voucher
/// `claim` — this contract's address is treated as the "fee miner", so every
/// mint still flows through the enclave-signed voucher path (no new mint
/// authority). A swap may be triggered ONLY by our ROFL enclave, gated via
/// `Subcall.roflEnsureAuthorizedOrigin(APP_ID)` — the SAME app-origin authority
/// `RentPayer` uses. This matters operationally: the swap is submitted through
/// `rofl-appd`'s sign-submit (app origin), so its GAS is paid by the funded app
/// account, not a separately-seeded operator EOA. (The old `onlyOperator`
/// design forced the enclave's voucher-signer EOA to hold gas, which silently
/// stalled swaps when it ran dry.) `minOut` (computed off-chain from a price
/// reference) is enforced by the router, so the pool never sells into a thin or
/// manipulated book. The ROSE is sent straight to `reservoir` (e.g. the
/// self-funding RentPayer), making the mining→rent loop autonomous.
contract FeeSwapper is Ownable {
    using SafeERC20 for IERC20;

    IERC20 public immutable miningPoolToken; // MPT
    IUniswapV2Router02 public immutable router;
    address public immutable wrappedNative; // WETH/WROSE, read from the router

    /// ROFL app id (21-byte bech32-decoded `rofl1…`). Immutable — the sole
    /// authority allowed to trigger a swap (origin = an authorized instance of
    /// this app). Same model as `RentPayer.APP_ID`.
    bytes21 public immutable APP_ID;
    /// Recipient of the swapped ROSE (rent reservoir / RentPayer).
    address public reservoir;

    event ReservoirUpdated(address indexed reservoir);
    event FeeSwapped(uint256 mptIn, uint256 minOut, uint256 roseOut, address indexed reservoir);

    constructor(
        IERC20 _miningPoolToken,
        IUniswapV2Router02 _router,
        bytes21 appId,
        address _reservoir
    ) Ownable(msg.sender) {
        require(
            address(_miningPoolToken) != address(0) &&
                address(_router) != address(0) &&
                _reservoir != address(0),
            "zero addr"
        );
        miningPoolToken = _miningPoolToken;
        router = _router;
        wrappedNative = _router.WETH();
        APP_ID = appId;
        reservoir = _reservoir;
        // Approve the router once to pull MPT for swaps.
        _miningPoolToken.forceApprove(address(_router), type(uint256).max);
        emit ReservoirUpdated(_reservoir);
    }

    function setReservoir(address _reservoir) external onlyOwner {
        require(_reservoir != address(0), "zero addr");
        reservoir = _reservoir;
        emit ReservoirUpdated(_reservoir);
    }

    /// @notice Off-chain helper: ROSE currently obtainable for `mptIn` MPT.
    /// The enclave uses this (plus its own slippage band) to size `minOut`.
    function quoteRoseOut(uint256 mptIn) external view returns (uint256) {
        uint[] memory amounts = router.getAmountsOut(mptIn, _path());
        return amounts[amounts.length - 1];
    }

    /// @notice Sell `mptIn` MPT for ROSE (≥ `minOut`) and forward the ROSE to
    /// the reservoir. Callable ONLY by our ROFL enclave (origin = an authorized
    /// instance of `APP_ID`); submit it via `rofl-appd` so the app account pays
    /// gas. The router reverts if output < `minOut`.
    function swapFeeToRose(uint256 mptIn, uint256 minOut, uint256 deadline)
        external
        returns (uint256 roseOut)
    {
        Subcall.roflEnsureAuthorizedOrigin(APP_ID);
        require(mptIn > 0, "mptIn=0");
        uint256 balBefore = reservoir.balance;
        router.swapExactTokensForETH(mptIn, minOut, _path(), reservoir, deadline);
        roseOut = reservoir.balance - balBefore;
        emit FeeSwapped(mptIn, minOut, roseOut, reservoir);
    }

    function _path() internal view returns (address[] memory path) {
        path = new address[](2);
        path[0] = address(miningPoolToken);
        path[1] = wrappedNative;
    }
}
