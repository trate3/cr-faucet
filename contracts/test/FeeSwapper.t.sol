// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";
import {MiningPoolToken} from "../src/MiningPoolToken.sol";
import {FeeSwapper} from "../src/FeeSwapper.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IUniswapV2Router02} from "../src/interfaces/IUniswapV2.sol";
import {RealUniswap} from "./helpers/RealUniswap.sol";

interface IRouterLiq {
    function addLiquidityETH(
        address token,
        uint amountTokenDesired,
        uint amountTokenMin,
        uint amountETHMin,
        address to,
        uint deadline
    ) external payable returns (uint amountToken, uint amountETH, uint liquidity);
}

contract FeeSwapperTest is RealUniswap {
    MiningPoolToken token;
    FeeSwapper swapper;
    address factory;
    address weth;
    address router;

    uint256 signerPk = 0xA11CE;
    address signer; // enclave EOA: the token's voucher signer (authorizedSigner)
    address reservoir = address(0x5E50); // rent reservoir
    address feeAccount; // = address(swapper)

    // Sapphire SUBCALL precompile + the ROFL app id the swapper is gated to.
    address constant SUBCALL = 0x0100000000000000000000000000000000000103;
    bytes21 constant APP_ID = bytes21(0xaabbccddeeff00112233445566778899aabbccddee);

    /// Make `roflEnsureAuthorizedOrigin(APP_ID)` pass: the precompile isn't
    /// available under foundry, so mock the SUBCALL staticcall to return the
    /// CBOR `true` (0xf5) the check expects.
    function _authorizeOrigin() internal {
        // roflEnsureAuthorizedOrigin staticcalls SUBCALL with
        // abi.encode("rofl.IsAuthorizedOrigin", 0x55||APP_ID) and expects
        // (status=0, data=0xf5 = CBOR true).
        vm.mockCall(
            SUBCALL,
            abi.encode("rofl.IsAuthorizedOrigin", abi.encodePacked(hex"55", APP_ID)),
            abi.encode(uint64(0), bytes(hex"f5"))
        );
    }

    function setUp() public {
        signer = vm.addr(signerPk);
        (factory, weth, router) = deployUniswapV2(address(this));

        // Token creates its MPT/WETH pair in the constructor, via the router.
        token = new MiningPoolToken(signer, 0, router);

        swapper = new FeeSwapper(
            IERC20(address(token)),
            IUniswapV2Router02(router),
            APP_ID, // enclave authority = ROFL app origin
            reservoir
        );
        feeAccount = address(swapper);

        // Seed the MPT/ROSE pool: 1,000,000 MPT (12dp) against 5 ROSE.
        _mint(address(this), 1_000_000_000_000);
        token.approve(router, type(uint256).max);
        vm.deal(address(this), 5 ether);
        IRouterLiq(router).addLiquidityETH{value: 5 ether}(
            address(token), 1_000_000_000_000, 0, 0, address(this), block.timestamp + 1
        );
    }

    function _voucher(address user, uint256 cum) internal view returns (bytes memory) {
        bytes32 typeHash = keccak256("Voucher(address user,uint256 cumulativeAmount,uint256 signedAt)");
        bytes32 structHash = keccak256(abi.encode(typeHash, user, cum, block.timestamp));
        bytes32 domain = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256(bytes("MiningPoolToken")),
            keccak256(bytes("1")),
            block.chainid,
            address(token)
        ));
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domain, structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerPk, digest);
        return abi.encodePacked(r, s, v);
    }

    /// Fee-MPT (or seed liquidity MPT) reaches a holder via the normal claim path.
    uint256 cumFor; // tracks cumulative per-recipient for monotonic vouchers
    mapping(address => uint256) cum;
    function _mint(address to, uint256 amount) internal {
        cum[to] += amount;
        bytes memory sig = _voucher(to, cum[to]);
        token.claim(to, cum[to], block.timestamp, sig);
    }

    function testTokenCreatesPairViaRouter() public view {
        assertTrue(token.pair() != address(0), "pair created in constructor");
    }

    function testSwapForwardsRealRoseToReservoir() public {
        _mint(feeAccount, 10_000_000_000); // 0.01 MPT-worth of fees (10e9 atomic)
        uint256 expectedOut = swapper.quoteRoseOut(10_000_000_000); // real constant-product quote
        assertGt(expectedOut, 0, "router quotes a real ROSE amount");

        uint256 before = reservoir.balance;
        _authorizeOrigin();
        uint256 out = swapper.swapFeeToRose(10_000_000_000, expectedOut, block.timestamp + 1);

        assertEq(out, expectedOut, "received the quoted ROSE");
        assertEq(reservoir.balance, before + expectedOut, "ROSE forwarded to reservoir");
        assertEq(token.balanceOf(feeAccount), 0, "fee-MPT sold into the pool");
    }

    /// Without an authorized ROFL origin (no SUBCALL precompile / mock), the swap
    /// must revert — only our enclave (app origin) may trigger it.
    function testUnauthorizedOriginReverts() public {
        _mint(feeAccount, 1_000_000);
        vm.expectRevert(); // RoflOriginNotAuthorizedForApp (or decode failure under foundry)
        swapper.swapFeeToRose(1_000_000, 0, block.timestamp + 1);
    }

    function testMinOutEnforcedByRealRouter() public {
        _mint(feeAccount, 1_000_000);
        uint256 fair = swapper.quoteRoseOut(1_000_000);
        _authorizeOrigin();
        vm.expectRevert(bytes("UniswapV2Router: INSUFFICIENT_OUTPUT_AMOUNT"));
        swapper.swapFeeToRose(1_000_000, fair + 1, block.timestamp + 1); // demand above market
    }

    function testSetReservoirOnlyOwner() public {
        vm.prank(address(0xBAD));
        vm.expectRevert();
        swapper.setReservoir(address(0x1234));
        // owner (this test contract) can repoint it
        swapper.setReservoir(address(0x9999));
        assertEq(swapper.reservoir(), address(0x9999));
    }

    function testAppIdImmutable() public view {
        assertEq(swapper.APP_ID(), APP_ID);
    }
}
