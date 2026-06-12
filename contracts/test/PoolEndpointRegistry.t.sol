// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";
import {PoolEndpointRegistry} from "../src/PoolEndpointRegistry.sol";

contract PoolEndpointRegistryTest is Test {
    PoolEndpointRegistry reg;

    // Sapphire SUBCALL precompile + the ROFL app id the registry is gated to.
    address constant SUBCALL = 0x0100000000000000000000000000000000000103;
    bytes21 constant APP_ID = bytes21(0xaabbccddeeff00112233445566778899aabbccddee);

    string constant ONION = "abc123def456ghi789jkl012mno345pqr678stu901vwx234yz567abc1d.onion";
    bytes32 constant FP = keccak256("stratum-tls-cert-der");

    /// Make `roflEnsureAuthorizedOrigin(APP_ID)` pass: mock the SUBCALL
    /// staticcall to return CBOR `true` (0xf5), since the precompile isn't
    /// available under foundry.
    function _authorizeOrigin() internal {
        vm.mockCall(
            SUBCALL,
            abi.encode("rofl.IsAuthorizedOrigin", abi.encodePacked(hex"55", APP_ID)),
            abi.encode(uint64(0), bytes(hex"f5"))
        );
    }

    function setUp() public {
        reg = new PoolEndpointRegistry(APP_ID);
    }

    function testEmptyInitially() public view {
        (string memory o, bytes32 fp, uint64 ts) = reg.endpoints();
        assertEq(bytes(o).length, 0);
        assertEq(fp, bytes32(0));
        assertEq(ts, 0);
    }

    function testAppIdImmutable() public view {
        assertEq(reg.APP_ID(), APP_ID);
    }

    function testSetByAuthorizedOrigin() public {
        _authorizeOrigin();
        vm.warp(1_700_000_000);
        reg.setEndpoints(ONION, FP);

        (string memory o, bytes32 fp, uint64 ts) = reg.endpoints();
        assertEq(o, ONION);
        assertEq(fp, FP);
        assertEq(ts, 1_700_000_000);
        // individual getters too
        assertEq(reg.onion(), ONION);
        assertEq(reg.tlsFingerprint(), FP);
    }

    function testUnauthorizedOriginReverts() public {
        // No mock installed → the SUBCALL check fails and the write reverts.
        vm.expectRevert();
        reg.setEndpoints(ONION, FP);
    }

    function testUpdateOverwrites() public {
        _authorizeOrigin();
        vm.warp(1_000);
        reg.setEndpoints(ONION, FP);
        bytes32 fp2 = keccak256("rotated-cert");
        vm.warp(2_000);
        reg.setEndpoints("new.onion", fp2);

        (string memory o, bytes32 fp, uint64 ts) = reg.endpoints();
        assertEq(o, "new.onion");
        assertEq(fp, fp2);
        assertEq(ts, 2_000);
    }
}
