// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Test} from "forge-std/Test.sol";

/// Deploys the REAL Uniswap V2 from its canonical compiled bytecode (vendored
/// under test/uniswap-artifacts/). We deploy bytecode rather than compiling the
/// 0.5.16/0.6.6 sources so they don't fight our 0.8.24 + via_ir build — and
/// because the official factory + router were compiled together, their pair
/// init-code-hash matches, so `swapExactTokensForETH`/`getAmountsOut` behave
/// exactly as on mainnet (real constant-product + 0.30% fee).
abstract contract RealUniswap is Test {
    function deployUniswapV2(address feeToSetter)
        internal
        returns (address factory, address weth, address router)
    {
        factory = _deploy("test/uniswap-artifacts/UniswapV2Factory.json", abi.encode(feeToSetter));
        weth = _deploy("test/uniswap-artifacts/WETH9.json", "");
        router = _deploy("test/uniswap-artifacts/UniswapV2Router02.json", abi.encode(factory, weth));
    }

    function _deploy(string memory artifact, bytes memory args) private returns (address addr) {
        bytes memory code = abi.encodePacked(vm.getCode(artifact), args);
        assembly {
            addr := create(0, add(code, 0x20), mload(code))
        }
        require(addr != address(0), "uniswap deploy failed");
    }
}
