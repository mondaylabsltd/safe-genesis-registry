// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test, console} from "forge-std/Test.sol";
import {SafeGenesisRegistry} from "../src/SafeGenesisRegistry.sol";

interface IProxyFactoryCode {
    function proxyCreationCode() external pure returns (bytes memory);
}

interface ISafeView {
    function getOwners() external view returns (address[] memory);
    function getThreshold() external view returns (uint256);
}

/// Real Safes and real Vela wallets, from `test/data/real-safes.json`
/// (built by `verify/`: Safes sampled from factory `ProxyCreation` events on
/// Gnosis and Base with their creation parameters recovered from the creating
/// transaction; Vela wallets from the public p256-index, derived with
/// vela-core's own `compute_safe_address_multi`).
///
/// The ground truth is never this registry's formula: for a Safe it is the
/// factory's own event ("factory F deployed proxy P"), for a Vela wallet the
/// address the wallet registered — and, in the fork tests, whatever the real
/// factory on another chain deploys when handed `genesisOf`'s fields.
contract RealSafesTest is Test {
    bytes4 internal constant SETUP = 0xb63e800d;

    struct Rows {
        address[] proxy;
        address[] factory;
        address[] singleton;
        bytes[] initializer;
        uint256[] saltNonce;
        uint256[] sourceChain;
        bool[] onGnosis;
        bool[] onBase;
        uint256[] codeIndex;
        bytes[] proxyCodes;
        string[] label;
    }

    struct Tally {
        uint256 replayed;
        uint256 ownersChecked;
        uint256 alreadyThere;
        uint256 expectedReverts;
        uint256 factoryMissing;
    }

    bytes32 internal constant FALLBACK_HANDLER_SLOT = 0x6c9a6c4a39284e37ed1cf53d337577d14212a4870fb976a4366c693b939918d5;
    bytes32 internal constant GUARD_SLOT = 0x4a204f620c8c5ccdca3fd54d003badd85ba500436a431f0cbda4f558c93c34c8;

    string internal constant SAMPLED = "test/data/real-safes.json";
    string internal constant KNOWN = "test/data/known-safes.json";
    string internal constant USER = "test/data/user-safes.json";

    function _rows(string memory path) internal view returns (Rows memory r) {
        string memory json = vm.readFile(path);
        r.proxy = vm.parseJsonAddressArray(json, ".proxy");
        r.factory = vm.parseJsonAddressArray(json, ".factory");
        r.singleton = vm.parseJsonAddressArray(json, ".singleton");
        r.initializer = vm.parseJsonBytesArray(json, ".initializer");
        r.saltNonce = vm.parseJsonUintArray(json, ".saltNonce");
        r.sourceChain = vm.parseJsonUintArray(json, ".sourceChain");
        r.onGnosis = vm.parseJsonBoolArray(json, ".onGnosis");
        r.onBase = vm.parseJsonBoolArray(json, ".onBase");
        r.codeIndex = vm.parseJsonUintArray(json, ".codeIndex");
        r.proxyCodes = vm.parseJsonBytesArray(json, ".proxyCodes");
        r.label = vm.parseJsonStringArray(json, ".label");
    }

    // -- no network ---------------------------------------------------------------

    /// Every real Safe and Vela wallet is filed under exactly its real
    /// address, and `genesisOf` returns exactly what went in.
    function test_SampledSafes_FiledUnderTheirRealAddress() public {
        _fileAll(SAMPLED);
    }

    function test_KnownSafes_FiledUnderTheirRealAddress() public {
        _fileAll(KNOWN);
    }

    function test_UserSafes_FiledUnderTheirRealAddress() public {
        _fileAll(USER);
    }

    function _fileAll(string memory path) internal {
        if (!vm.exists(path)) vm.skip(true); // user-safes.json is local only
        Rows memory r = _rows(path);
        SafeGenesisRegistry registry = new SafeGenesisRegistry();
        for (uint256 i = 0; i < r.proxy.length; ++i) {
            (address safe, bool created) = registry.record(
                r.factory[i], r.proxyCodes[r.codeIndex[i]], r.singleton[i], r.initializer[i], r.saltNonce[i], "Real Safe"
            );
            assertEq(safe, r.proxy[i], r.label[i]);
            assertTrue(created, r.label[i]);

            (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(r.proxy[i]);
            assertTrue(found, r.label[i]);
            assertEq(g.factory, r.factory[i], r.label[i]);
            assertEq(g.singleton, r.singleton[i], r.label[i]);
            assertEq(g.saltNonce, r.saltNonce[i], r.label[i]);
            assertEq(g.initializer, r.initializer[i], r.label[i]);
            assertEq(g.proxyCreationCodeHash, keccak256(r.proxyCodes[r.codeIndex[i]]), r.label[i]);
        }
        assertEq(registry.totalRecords(), r.proxy.length);
        console.log(path, "rows filed under their real address:", r.proxy.length);
    }

    // -- forks (opt-in: FORK_RPC_GNOSIS, FORK_RPC_BASE) ----------------------------

    function test_Fork_SampledSafes_DeployFromGenesisOnBase() public {
        _deployFromGenesisOn(SAMPLED, vm.envOr("FORK_RPC_BASE", string("")), 8453);
    }

    function test_Fork_SampledSafes_DeployFromGenesisOnGnosis() public {
        _deployFromGenesisOn(SAMPLED, vm.envOr("FORK_RPC_GNOSIS", string("")), 100);
    }

    function test_Fork_KnownSafes_DeployFromGenesisOnBase() public {
        _deployFromGenesisOn(KNOWN, vm.envOr("FORK_RPC_BASE", string("")), 8453);
    }

    function test_Fork_KnownSafes_DeployFromGenesisOnGnosis() public {
        _deployFromGenesisOn(KNOWN, vm.envOr("FORK_RPC_GNOSIS", string("")), 100);
    }

    function test_Fork_UserSafes_DeployFromGenesisOnBase() public {
        _deployFromGenesisOn(USER, vm.envOr("FORK_RPC_BASE", string("")), 8453);
    }

    function test_Fork_UserSafes_DeployFromGenesisOnGnosis() public {
        _deployFromGenesisOn(USER, vm.envOr("FORK_RPC_GNOSIS", string("")), 100);
    }

    /// On the target chain: file each row, read it back with `genesisOf`, and
    /// deploy from those fields alone. Rows from the other chain and every
    /// Vela wallet take part. A Safe that already exists there is counted —
    /// the same address on another chain is the same genesis.
    function _deployFromGenesisOn(string memory path, string memory rpcUrl, uint256 chainId) internal {
        if (bytes(rpcUrl).length == 0 || !vm.exists(path)) vm.skip(true);
        Rows memory r = _rows(path);
        vm.createSelectFork(rpcUrl);
        assertEq(block.chainid, chainId);
        SafeGenesisRegistry registry = new SafeGenesisRegistry();
        Tally memory t;

        for (uint256 i = 0; i < r.proxy.length; ++i) {
            if (r.sourceChain[i] == chainId) continue; // only carry rows ACROSS chains
            bool there = chainId == 100 ? r.onGnosis[i] : r.onBase[i];
            if (there) {
                assertGt(r.proxy[i].code.length, 0, r.label[i]);
                ++t.alreadyThere;
                continue;
            }
            registry.record(
                r.factory[i], r.proxyCodes[r.codeIndex[i]], r.singleton[i], r.initializer[i], r.saltNonce[i], "Real Safe"
            );
            _deployOne(registry, r.proxy[i], r.label[i], t);
        }

        console.log(path, "on chain", chainId);
        console.log("  deployed from genesisOf at the same address:", t.replayed);
        console.log("    of which owners + threshold equal the initializer's:", t.ownersChecked);
        console.log("  already deployed there (same address):", t.alreadyThere);
        console.log("  reverted as predicted (a setup that can never succeed):", t.expectedReverts);
        console.log("  factory missing or different proxy code:", t.factoryMissing);
        assertGt(t.replayed + t.alreadyThere, 0);
    }

    function _deployOne(SafeGenesisRegistry registry, address proxy, string memory label, Tally memory t) internal {
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(proxy);
        assertTrue(found, label);
        if (
            g.factory.code.length == 0
                || keccak256(IProxyFactoryCode(g.factory).proxyCreationCode()) != g.proxyCreationCodeHash
        ) {
            ++t.factoryMissing;
            return;
        }
        assertEq(proxy.code.length, 0, label);

        // Only what genesisOf returned.
        (bool ok, bytes memory ret) =
            g.factory.call(abi.encodeWithSelector(0x1688f0b9, g.singleton, g.initializer, g.saltNonce));
        // A revert is acceptable only where the setup predictably can never
        // succeed (a zero or sentinel owner, a duplicate, a bad threshold).
        bool doomed = _doomed(g.initializer);
        if (!ok) {
            assertTrue(doomed, string.concat("unexpected revert: ", label));
            ++t.expectedReverts;
            return;
        }
        assertFalse(doomed, string.concat("predicted to revert but deployed: ", label));
        assertEq(abi.decode(ret, (address)), proxy, label);
        assertGt(proxy.code.length, 0, label);
        ++t.replayed;

        if (bytes4(g.initializer) != SETUP) return;
        (address[] memory owners, uint256 threshold) = abi.decode(_args(g.initializer), (address[], uint256));
        assertEq(ISafeView(proxy).getOwners(), owners, label);
        assertEq(ISafeView(proxy).getThreshold(), threshold, label);
        // The rest of its authority: the fallback handler setup asked for, no guard.
        address handler = address(uint160(uint256(_word(g.initializer, 4 + 4 * 32))));
        assertEq(address(uint160(uint256(vm.load(proxy, FALLBACK_HANDLER_SLOT)))), handler, label);
        assertEq(vm.load(proxy, GUARD_SLOT), bytes32(0), label);
        ++t.ownersChecked;
    }

    function _word(bytes memory b, uint256 at) internal pure returns (bytes32 w) {
        assembly {
            w := mload(add(add(b, 0x20), at))
        }
    }

    /// Safe's `setupOwners` refusals, predicted from the initializer.
    function _doomed(bytes memory initializer) internal pure returns (bool) {
        if (bytes4(initializer) != SETUP) return false;
        (address[] memory owners, uint256 threshold) = abi.decode(_args(initializer), (address[], uint256));
        if (threshold == 0 || threshold > owners.length) return true;
        for (uint256 i = 0; i < owners.length; ++i) {
            if (owners[i] == address(0) || owners[i] == address(1)) return true;
            for (uint256 j = 0; j < i; ++j) {
                if (owners[j] == owners[i]) return true;
            }
        }
        return false;
    }

    /// `setup`'s first two arguments, re-wrapped as a standalone ABI tuple.
    function _args(bytes memory initializer) internal pure returns (bytes memory out) {
        out = new bytes(initializer.length - 4);
        for (uint256 i = 0; i < out.length; ++i) {
            out[i] = initializer[i + 4];
        }
    }
}
