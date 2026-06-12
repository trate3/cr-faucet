// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";
import {MiningPoolToken} from "../src/MiningPoolToken.sol";

contract MiningPoolTokenTest is Test {
    MiningPoolToken token;
    uint256 signerPk = 0xA11CE;
    address signer;
    address miner = address(0xBEEF);

    function setUp() public {
        signer = vm.addr(signerPk);
        // Subsidy 0 by default so the claim/redeem/markProcessed tests stay
        // focused; the enforcement tests below set it explicitly.
        token = new MiningPoolToken(signer, 0, address(0));
    }

    function _voucher(address user, uint256 cum, uint256 signedAt) internal view returns (bytes memory) {
        bytes32 typeHash = keccak256("Voucher(address user,uint256 cumulativeAmount,uint256 signedAt)");
        bytes32 structHash = keccak256(abi.encode(typeHash, user, cum, signedAt));
        bytes32 domain = _domain();
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domain, structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerPk, digest);
        return abi.encodePacked(r, s, v);
    }

    function _domain() internal view returns (bytes32) {
        return keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256(bytes("MiningPoolToken")),
            keccak256(bytes("1")),
            block.chainid,
            address(token)
        ));
    }

    function testClaimMintsDelta() public {
        bytes memory sig = _voucher(miner, 100, block.timestamp + 1 hours);
        token.claim(miner, 100, block.timestamp + 1 hours, sig);
        assertEq(token.balanceOf(miner), 100);
        assertEq(token.claimed(miner), 100);
    }

    function testClaimAgainMintsOnlyDelta() public {
        bytes memory sig1 = _voucher(miner, 100, block.timestamp + 1 hours);
        token.claim(miner, 100, block.timestamp + 1 hours, sig1);
        bytes memory sig2 = _voucher(miner, 150, block.timestamp + 1 hours);
        token.claim(miner, 150, block.timestamp + 1 hours, sig2);
        assertEq(token.balanceOf(miner), 150);
        assertEq(token.claimed(miner), 150);
    }

    function testReplayOldVoucherReverts() public {
        bytes memory sig1 = _voucher(miner, 100, block.timestamp + 1 hours);
        token.claim(miner, 100, block.timestamp + 1 hours, sig1);
        // Now miner has cumulative 100; resubmitting same voucher must fail.
        vm.expectRevert(bytes("no new balance"));
        token.claim(miner, 100, block.timestamp + 1 hours, sig1);
    }

    function testWrongSignerReverts() public {
        // sign with a different pk
        uint256 evilPk = 0xBAD;
        bytes32 typeHash = keccak256("Voucher(address user,uint256 cumulativeAmount,uint256 signedAt)");
        bytes32 structHash = keccak256(abi.encode(typeHash, miner, uint256(100), block.timestamp + 1 hours));
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", _domain(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(evilPk, digest);
        bytes memory sig = abi.encodePacked(r, s, v);
        vm.expectRevert(bytes("bad voucher sig"));
        token.claim(miner, 100, block.timestamp + 1 hours, sig);
    }

    /// `signedAt` is recorded in the voucher but NOT enforced on-chain, so a
    /// voucher signed arbitrarily far in the past is still claimable. (Double-
    /// mint protection comes from the cumulative watermark, not a time window.)
    function testStaleSignedAtStillClaims() public {
        uint256 signedAt = 1000; // fixed past sign-time
        bytes memory sig = _voucher(miner, 100, signedAt);
        vm.warp(signedAt + 365 days); // long after; no window is enforced
        token.claim(miner, 100, signedAt, sig);
        assertEq(token.balanceOf(miner), 100);
    }

    function testRedeemBurnsAndEmits() public {
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.prank(miner);
        vm.expectEmit(true, false, false, false);
        emit MiningPoolToken.Redemption(miner, 400, "44addr...", 1);
        token.redeem(400, "44addr...");
        assertEq(token.balanceOf(miner), 600);
    }

    /// `redeem` forwards any attached native value to the signer (the
    /// enclave account) so it can fund the `markProcessed` tx.
    function testRedeemForwardsGasSubsidyToSigner() public {
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.deal(miner, 1 ether);
        uint256 before = signer.balance;
        vm.prank(miner);
        token.redeem{value: 0.01 ether}(400, "44addr...");
        assertEq(signer.balance, before + 0.01 ether, "gas subsidy forwarded to signer");
    }

    /// Only the enclave signer can mark a redemption processed; the flag
    /// + txid are recorded and the event fires.
    function testMarkProcessedOnlySigner() public {
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.prank(miner);
        token.redeem(400, "44addr...");

        // Non-signer is rejected.
        vm.prank(miner);
        vm.expectRevert("not authorized");
        token.markProcessed(1, "monero-tx-1", 0);

        // Signer succeeds, sets flag + txid + event.
        vm.prank(signer);
        vm.expectEmit(true, false, false, true);
        emit MiningPoolToken.RedemptionProcessed(1, "monero-tx-1");
        token.markProcessed(1, "monero-tx-1", 0);
        assertTrue(token.processed(1));
        assertEq(token.payoutTxid(1), "monero-tx-1");
    }

    /// Re-marking a processed id is a no-op that keeps the first txid.
    function testMarkProcessedIdempotent() public {
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.prank(miner);
        token.redeem(400, "44addr...");
        vm.prank(signer);
        token.markProcessed(1, "first-tx", 0);
        vm.prank(signer);
        token.markProcessed(1, "second-tx", 0); // ignored
        assertEq(token.payoutTxid(1), "first-tx");
    }

    function testMarkProcessedRejectsUnknownId() public {
        vm.prank(signer);
        vm.expectRevert("unknown redemption");
        token.markProcessed(99, "x", 0);
    }

    /// markProcessed advances restoreHeight monotonically in the same tx, and
    /// mark-only (height 0) / lower values leave it unchanged.
    function testMarkProcessedAdvancesRestoreHeight() public {
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.startPrank(miner);
        token.redeem(100, "44a");
        token.redeem(100, "44b");
        token.redeem(100, "44c");
        vm.stopPrank();

        assertEq(token.restoreHeight(), 0);
        vm.startPrank(signer);
        token.markProcessed(1, "tx1", 500); // mark + advance
        assertEq(token.restoreHeight(), 500);
        token.markProcessed(2, "tx2", 0); // mark only — height unchanged
        assertEq(token.restoreHeight(), 500);
        token.markProcessed(3, "tx3", 400); // lower — ignored (monotonic)
        assertEq(token.restoreHeight(), 500);
        token.markProcessed(3, "tx3b", 900); // re-mark id 3, but height rides along
        assertEq(token.restoreHeight(), 900);
        assertEq(token.payoutTxid(3), "tx3"); // first txid kept (idempotent mark)
        vm.stopPrank();
    }

    /// When a subsidy is set, a redemption with too little attached value
    /// reverts — you cannot create a redemption that can't fund its own mark.
    function testRedeemRevertsWhenSubsidyUnderfunded() public {
        token.setRedemptionGasSubsidy(0.01 ether);
        bytes memory sig = _voucher(miner, 1000, block.timestamp + 1 hours);
        token.claim(miner, 1000, block.timestamp + 1 hours, sig);
        vm.deal(miner, 1 ether);

        vm.prank(miner);
        vm.expectRevert("redeem: gas subsidy required");
        token.redeem{value: 0.01 ether - 1}(400, "44addr...");

        // Exactly the subsidy is accepted, and it lands on the signer.
        uint256 before = signer.balance;
        vm.prank(miner);
        token.redeem{value: 0.01 ether}(400, "44addr...");
        assertEq(signer.balance, before + 0.01 ether);
        assertEq(token.balanceOf(miner), 600);
    }

    function testSetRedemptionGasSubsidyOnlyOwner() public {
        vm.prank(miner);
        vm.expectRevert();
        token.setRedemptionGasSubsidy(1 ether);

        token.setRedemptionGasSubsidy(0.02 ether);
        assertEq(token.redemptionGasSubsidy(), 0.02 ether);
    }
}
