// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";
import {RentPayer} from "../src/RentPayer.sol";

/// RentPayer's `topUp()` relies on the Sapphire SUBCALL precompile
/// (roflEnsureAuthorizedOrigin + roflmarket.InstanceTopUp), which isn't
/// available under `forge test` — that path is exercised on sapphire-localnet /
/// testnet. Here we validate the parts that ARE pure EVM: the CBOR encoding
/// (byte-for-byte vs the oasis CLI's verified ground truth), the input guards,
/// and funding via receive().
contract RentPayerTest is Test {
    // Fixed fixtures so the expected CBOR is a concrete literal.
    bytes21 constant APP_ID = bytes21(0xaabbccddeeff00112233445566778899aabbccddee);
    bytes21 constant PROVIDER = bytes21(0x0102030405060708090a0b0c0d0e0f101112131415);
    bytes8 constant INSTANCE_ID = bytes8(0x0000000000000624); // machine 0000000000000624

    RentPayer rp;

    function setUp() public {
        rp = new RentPayer(APP_ID, PROVIDER, INSTANCE_ID);
    }

    /// The canonical `roflmarket.InstanceTopUp` body for term=hour, count=1.
    /// This literal is the byte layout verified in
    /// research/rofl-trustless-faucet/05-verified-architecture.md against
    /// `oasis rofl machine top-up --offline --unsigned` — map(4), length-first
    /// key order id(2)/term(4)/provider(8)/term_count(10), bare minimal uints.
    function testEncodeTopUpBodyMatchesCliGroundTruth() public view {
        // map(4) | "id"(62 6964) bstr8(48)+instanceid | "term"(64 7465726d) uint(01)
        //        | "provider"(68 70726f7669646572) bstr21(55)+provider
        //        | "term_count"(6a 7465726d5f636f756e74) uint(01)
        bytes memory expected =
            hex"a4626964480000000000000624647465726d016870726f7669646572550102030405060708090a0b0c0d0e0f1011121314156a7465726d5f636f756e7401";
        assertEq(rp.encodeTopUpBody(1, 1), expected);
        // 62-byte body: 1 + (1+2+1+8) + (1+4+1) + (1+8+1+21) + (1+10+1).
        assertEq(rp.encodeTopUpBody(1, 1).length, 62);
    }

    /// A different term/count flips exactly the two bare-uint bytes.
    function testEncodeTopUpBodyVariesTermAndCount() public view {
        bytes memory b = rp.encodeTopUpBody(2, 23); // month, 23
        // term byte is at offset 1+1+2+1+8 + (1+4) = 18; term_count is the last byte.
        assertEq(uint8(b[18]), 2, "term byte");
        assertEq(uint8(b[b.length - 1]), 23, "term_count byte");
    }

    function testGuardsRejectOutOfRange() public {
        vm.expectRevert("term must be 1..3");
        rp.encodeTopUpBody(0, 1);
        vm.expectRevert("term must be 1..3");
        rp.encodeTopUpBody(4, 1);
        vm.expectRevert("term_count must be 1..23");
        rp.encodeTopUpBody(1, 0);
        vm.expectRevert("term_count must be 1..23");
        rp.encodeTopUpBody(1, 24);
    }

    function testInitialTargetingPinned() public view {
        assertEq(rp.APP_ID(), APP_ID);
        assertEq(rp.provider(), PROVIDER);
        assertEq(rp.instanceId(), INSTANCE_ID);
    }

    /// setInstance is origin-gated (Subcall.roflEnsureAuthorizedOrigin), which
    /// isn't available under foundry — so here it must revert (no precompile),
    /// confirming a plain caller can't retarget. The app-origin happy path is
    /// exercised on sapphire-localnet / testnet.
    function testSetInstanceNeedsAppOrigin() public {
        vm.expectRevert();
        rp.setInstance(PROVIDER, bytes8(0x0000000000000999));
    }

    function testAnyoneCanFund() public {
        address funder = address(0xF00D);
        vm.deal(funder, 5 ether);
        vm.prank(funder);
        (bool ok, ) = address(rp).call{value: 3 ether}("");
        assertTrue(ok, "funding should succeed");
        assertEq(address(rp).balance, 3 ether);
    }
}
