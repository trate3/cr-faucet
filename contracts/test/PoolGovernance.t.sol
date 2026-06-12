// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";
import {PoolGovernance} from "../src/PoolGovernance.sol";
import {MiningPoolToken} from "../src/MiningPoolToken.sol";

contract PoolGovernanceTest is Test {
    PoolGovernance gov;
    MiningPoolToken token;
    address governor;
    uint256 delay = 2 days;

    function setUp() public {
        governor = address(0x6011);
        gov = new PoolGovernance(governor, delay);
        token = new MiningPoolToken(vm.addr(0xA11CE), 0, address(0));
        // Hand the token's admin surface to governance.
        token.transferOwnership(address(gov));
        assertEq(token.owner(), address(gov));
    }

    function _setSubsidyCall(uint256 v) internal pure returns (bytes memory) {
        return abi.encodeWithSelector(MiningPoolToken.setRedemptionGasSubsidy.selector, v);
    }

    function testQueueThenExecuteAfterDelay() public {
        bytes memory data = _setSubsidyCall(0.05 ether);
        vm.prank(governor);
        gov.queue(address(token), 0, data);

        // Too early.
        vm.prank(governor);
        vm.expectRevert("timelock");
        gov.execute(address(token), 0, data);

        // After the delay it lands on the token.
        vm.warp(block.timestamp + delay);
        vm.prank(governor);
        gov.execute(address(token), 0, data);
        assertEq(token.redemptionGasSubsidy(), 0.05 ether);
    }

    function testNonGovernorRejected() public {
        bytes memory data = _setSubsidyCall(1);
        vm.expectRevert("not governor");
        gov.queue(address(token), 0, data);
    }

    function testExecuteRequiresQueue() public {
        bytes memory data = _setSubsidyCall(1);
        vm.warp(block.timestamp + delay);
        vm.prank(governor);
        vm.expectRevert("not queued");
        gov.execute(address(token), 0, data);
    }

    function testCancel() public {
        bytes memory data = _setSubsidyCall(1);
        vm.startPrank(governor);
        gov.queue(address(token), 0, data);
        gov.cancel(address(token), 0, data);
        vm.warp(block.timestamp + delay);
        vm.expectRevert("not queued");
        gov.execute(address(token), 0, data);
        vm.stopPrank();
    }

    function testRenounceFreezesAdminButTokenStillWorks() public {
        vm.prank(governor);
        gov.renounce();

        // No more governance actions ever.
        bytes memory data = _setSubsidyCall(1);
        vm.prank(governor);
        vm.expectRevert("renounced");
        gov.queue(address(token), 0, data);

        // The token's owner is still the (now-frozen) governance, so the
        // owner-only surface is permanently uncallable...
        assertEq(token.owner(), address(gov));
        // ...but ordinary use is unaffected (no owner needed): a voucher claim.
        uint256 pk = 0xA11CE; // matches the signer set in setUp
        address miner = address(0xBEEF);
        bytes memory sig = _voucher(pk, miner, 100);
        token.claim(miner, 100, block.timestamp, sig);
        assertEq(token.balanceOf(miner), 100);
    }

    function testTransferGovernor() public {
        address next = address(0x6022);
        vm.prank(governor);
        gov.transferGovernor(next);
        assertEq(gov.governor(), next);
        // Old governor can no longer act.
        vm.prank(governor);
        vm.expectRevert("not governor");
        gov.queue(address(token), 0, _setSubsidyCall(1));
    }

    function _voucher(uint256 pk, address user, uint256 cum) internal view returns (bytes memory) {
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
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }
}
