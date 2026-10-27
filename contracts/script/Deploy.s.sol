// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {SafeGenesisRegistry} from "../src/SafeGenesisRegistry.sol";

/// Deploys the registry through the deterministic CREATE2 deployer
/// (0x4e59b44847b379578588920cA78FbF26c0B4956C), so it lands at the same
/// address on every chain built from the same bytecode and salt. The
/// registry has no constructor arguments and no owner: the address depends
/// only on the compiled code (see foundry.toml: no metadata hash) and SALT.
///
///   forge script script/Deploy.s.sol --rpc-url $RPC                        # predict only
///   forge script script/Deploy.s.sol --rpc-url $RPC --broadcast --account deployer
///
/// Anyone may run this on a new chain; the result is the same contract at the
/// same address. Check first that the deterministic deployer exists there:
///   cast code 0x4e59b44847b379578588920cA78FbF26c0B4956C --rpc-url $RPC
contract DeployScript is Script {
    /// Fixed forever: changing it moves the registry on every chain.
    bytes32 internal constant SALT = keccak256("safe-genesis-registry/v1");

    function predicted() public pure returns (address) {
        return vm.computeCreate2Address(SALT, keccak256(type(SafeGenesisRegistry).creationCode), CREATE2_FACTORY);
    }

    function run() public {
        address target = predicted();
        console.log("SafeGenesisRegistry address:", target);
        console.log("init code hash:");
        console.logBytes32(keccak256(type(SafeGenesisRegistry).creationCode));

        if (target.code.length != 0) {
            console.log("already deployed on chain", block.chainid);
            return;
        }
        require(CREATE2_FACTORY.code.length != 0, "deterministic deployer missing on this chain");

        vm.startBroadcast();
        SafeGenesisRegistry registry = new SafeGenesisRegistry{salt: SALT}();
        vm.stopBroadcast();

        require(address(registry) == target, "deployed off the predicted address");
        console.log("deployed on chain", block.chainid);
    }
}
