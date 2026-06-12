// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {MiningPoolToken} from "../src/MiningPoolToken.sol";
import {FeeSwapper} from "../src/FeeSwapper.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IUniswapV2Router02} from "../src/interfaces/IUniswapV2.sol";

interface IRouterLiq {
    function addLiquidityETH(
        address token,
        uint amountTokenDesired,
        uint amountTokenMin,
        uint amountETHMin,
        address to,
        uint deadline
    ) external payable returns (uint, uint, uint);
}

/// @notice One-shot deploy of the fee→ROSE stack, driven entirely by a TOML
/// config file (no bash glue). Reads `DEPLOY_CONFIG` (default
/// ../deploy/fee_swap.deploy.toml) for all parameters; the only env var is the
/// secret `DEPLOYER_PK`, kept out of the config.
///
/// Run (auto-verifies our contracts on Sourcify):
///   DEPLOYER_PK=0x… forge script script/DeployFeeSwap.s.sol \
///       --rpc-url <url> --legacy --broadcast --verify --verifier sourcify
///
/// Config (TOML), amounts as decimal STRINGS (TOML ints are i64 — wei overflows):
///   [dex]    mode = "create" | "supply"     router = "0x…"   (supply only)
///   [token]  signer = "0x…"   redemption_gas_subsidy = "20000000000000000"
///   [fee_swapper] reservoir = "0x…"   app_id = "0x…" (21-byte ROFL app id)
///   [seed]   mpt = "1000000000000"   rose_wei = "1000000000000000000"  (0 to skip)
contract DeployFeeSwap is Script {
    function run() external {
        uint256 pk = vm.envUint("DEPLOYER_PK");
        address deployer = vm.addr(pk);
        string memory cfgPath = vm.envOr("DEPLOY_CONFIG", string("../deploy/fee_swap.deploy.toml"));
        string memory t = vm.readFile(cfgPath);

        string memory mode = vm.parseTomlString(t, ".dex.mode");
        address signer = vm.parseTomlAddress(t, ".token.signer");
        uint256 subsidy = vm.parseUint(vm.parseTomlString(t, ".token.redemption_gas_subsidy"));
        address reservoir = vm.parseTomlAddress(t, ".fee_swapper.reservoir");
        // ROFL app id (21-byte bech32-decoded rofl1…) — the swap's app-origin
        // authority, mirroring RentPayer. deploy_contracts.sh writes the hex.
        bytes memory appIdRaw = vm.parseTomlBytes(t, ".fee_swapper.app_id");
        require(appIdRaw.length == 21, "app_id must be 21 bytes");
        bytes21 appId;
        assembly { appId := mload(add(appIdRaw, 0x20)) }
        uint256 seedMpt = vm.parseUint(vm.parseTomlString(t, ".seed.mpt"));
        uint256 seedRose = vm.parseUint(vm.parseTomlString(t, ".seed.rose_wei"));

        vm.startBroadcast(pk);

        address router;
        if (_eq(mode, "create")) {
            address factory = _deploy("test/uniswap-artifacts/UniswapV2Factory.json", abi.encode(deployer));
            // Pair MPT against the canonical Wrapped ROSE when configured
            // (mainnet 0x8Bc2…D2D3 / testnet 0xB759…3b94); only fall back to a
            // throwaway WETH9 for self-contained local/CI (wrapped_native = 0x0).
            address weth = vm.parseTomlAddress(t, ".dex.wrapped_native");
            if (weth == address(0)) weth = _deploy("test/uniswap-artifacts/WETH9.json", "");
            router = _deploy("test/uniswap-artifacts/UniswapV2Router02.json", abi.encode(factory, weth));
            console2.log("UniswapV2Factory:", factory);
            console2.log("WrappedNative:   ", weth);
        } else {
            router = vm.parseTomlAddress(t, ".dex.router");
        }
        console2.log("UniswapV2Router: ", router);

        MiningPoolToken token = new MiningPoolToken(signer, subsidy, router);
        console2.log("MiningPoolToken: ", address(token));
        console2.log("MPT/WROSE pair:  ", token.pair());

        FeeSwapper swapper =
            new FeeSwapper(IERC20(address(token)), IUniswapV2Router02(router), appId, reservoir);
        console2.log("FeeSwapper:      ", address(swapper));

        if (seedMpt > 0 && seedRose > 0) {
            require(signer == deployer, "seed needs signer==deployer to sign the voucher");
            bytes memory sig = _voucher(pk, address(token), deployer, seedMpt);
            token.claim(deployer, seedMpt, block.timestamp, sig);
            token.approve(router, seedMpt);
            IRouterLiq(router).addLiquidityETH{value: seedRose}(
                address(token), seedMpt, 0, 0, deployer, block.timestamp + 600
            );
            console2.log("seeded MPT:      ", seedMpt);
            console2.log("seeded ROSE wei: ", seedRose);
        }

        vm.stopBroadcast();
    }

    function _voucher(uint256 pk, address token, address user, uint256 cum)
        internal
        view
        returns (bytes memory)
    {
        bytes32 typeHash = keccak256("Voucher(address user,uint256 cumulativeAmount,uint256 signedAt)");
        bytes32 structHash = keccak256(abi.encode(typeHash, user, cum, block.timestamp));
        bytes32 domain = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256(bytes("MiningPoolToken")),
            keccak256(bytes("1")),
            block.chainid,
            token
        ));
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domain, structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    function _deploy(string memory artifact, bytes memory args) internal returns (address addr) {
        bytes memory code = abi.encodePacked(vm.getCode(artifact), args);
        assembly {
            addr := create(0, add(code, 0x20), mload(code))
        }
        require(addr != address(0), "uniswap deploy failed");
    }

    function _eq(string memory a, string memory b) internal pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
}
