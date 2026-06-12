// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";
import {RoflAdminGovernance} from "../src/RoflAdminGovernance.sol";
import {Subcall} from "sapphire-contracts/Subcall.sol";

contract RoflAdminGovernanceTest is Test {
    RoflAdminGovernance gov;
    address governor = address(0x6011);
    uint256 delay = 2 days;
    bytes body;

    function setUp() public {
        gov = new RoflAdminGovernance(governor, delay);
        body = bytes("rofl-update-cbor-body");
    }

    function _mockSubcall(uint64 status) internal {
        // Subcall.subcall does SUBCALL.call(abi.encode(method, body)); mock that.
        vm.mockCall(
            Subcall.SUBCALL,
            abi.encode("rofl.Update", body),
            abi.encode(status, bytes(""))
        );
    }

    function testProposeRequiresGovernor() public {
        vm.expectRevert("not governor");
        gov.propose(body);
    }

    function testTimelockThenSuccess() public {
        vm.prank(governor);
        gov.propose(body);
        assertEq(gov.pendingHash(), keccak256(body));

        // too early
        vm.prank(governor);
        vm.expectRevert("timelock");
        gov.execute(body);

        // after the delay, relay succeeds (mocked status 0)
        vm.warp(block.timestamp + delay);
        _mockSubcall(0);
        vm.prank(governor);
        gov.execute(body);
        assertEq(gov.eta(), 0);
        assertEq(gov.pendingHash(), bytes32(0));
    }

    function testExecuteBodyMismatch() public {
        vm.prank(governor);
        gov.propose(body);
        vm.warp(block.timestamp + delay);
        vm.prank(governor);
        vm.expectRevert("body mismatch");
        gov.execute(bytes("different-body"));
    }

    function testExecuteRevertsOnNonzeroStatus() public {
        vm.prank(governor);
        gov.propose(body);
        vm.warp(block.timestamp + delay);
        _mockSubcall(7); // runtime returned an error status
        vm.prank(governor);
        vm.expectRevert("rofl.Update failed");
        gov.execute(body);
    }

    function testCancelClearsPending() public {
        vm.startPrank(governor);
        gov.propose(body);
        gov.cancel();
        assertEq(gov.eta(), 0);
        vm.warp(block.timestamp + delay);
        vm.expectRevert("timelock"); // nothing pending
        gov.execute(body);
        vm.stopPrank();
    }

    function testTransferGovernor() public {
        vm.prank(governor);
        gov.transferGovernor(address(0x6022));
        assertEq(gov.governor(), address(0x6022));
        vm.prank(governor);
        vm.expectRevert("not governor");
        gov.propose(body);
    }
}
