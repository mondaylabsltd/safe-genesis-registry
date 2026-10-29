// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {SafeGenesisRegistry} from "../src/SafeGenesisRegistry.sol";

interface ISafeProxyFactory {
    function proxyCreationCode() external pure returns (bytes memory);
    function createProxyWithNonce(address singleton, bytes memory initializer, uint256 saltNonce)
        external
        returns (address proxy);
}

interface ISafe {
    function setup(
        address[] calldata owners,
        uint256 threshold,
        address to,
        bytes calldata data,
        address fallbackHandler,
        address paymentToken,
        uint256 payment,
        address payable paymentReceiver
    ) external;
    function swapOwner(address prevOwner, address oldOwner, address newOwner) external;
    function getOwners() external view returns (address[] memory);
    function getThreshold() external view returns (uint256);
    function VERSION() external view returns (string memory);
}

/// Against the canonical Safe deployments, on real chain state. Opt-in:
///
///   FORK_RPC_GNOSIS=https://rpc.gnosischain.com FORK_RPC_BASE=https://mainnet.base.org forge test --mt Fork
///
/// Each test creates a Safe on Gnosis and records its genesis, changes an
/// owner there, reads the genesis back by the Safe's address, and deploys it
/// on Base: same address, original owners.
contract ForkSafeTest is Test {
    // Safe v1.3.0 (canonical deployments)
    address internal constant FACTORY_130 = 0xa6B71E26C5e0845f74c812102Ca7114b6a896AB2;
    address internal constant SAFE_L2_130 = 0x3E5c63644E683549055b9Be8653de26E0B4CD36E;
    address internal constant HANDLER_130 = 0xf48f2B2d2a534e402487b3ee7C18c33Aec0Fe5e4;
    // Safe v1.4.1 (canonical deployments)
    address internal constant FACTORY_141 = 0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67;
    address internal constant SAFE_L2_141 = 0x29fcB43b46531BcA003ddC8FCB67FFE91900C762;
    address internal constant HANDLER_141 = 0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99;

    string internal gnosisRpc;
    string internal baseRpc;

    function setUp() public {
        gnosisRpc = vm.envOr("FORK_RPC_GNOSIS", string(""));
        baseRpc = vm.envOr("FORK_RPC_BASE", string(""));
    }

    function _skipWithoutRpc() internal {
        if (bytes(gnosisRpc).length == 0 || bytes(baseRpc).length == 0) vm.skip(true);
    }

    function _owners() internal returns (address[] memory owners) {
        owners = new address[](3);
        owners[0] = makeAddr("owner-a");
        owners[1] = makeAddr("owner-b");
        owners[2] = makeAddr("owner-c");
    }

    function _setup(address[] memory owners, address handler) internal pure returns (bytes memory) {
        return abi.encodeCall(ISafe.setup, (owners, 2, address(0), "", handler, address(0), 0, payable(address(0))));
    }

    function _carryAcrossChains(address factory, address singleton, address handler, string memory version) internal {
        _skipWithoutRpc();
        address[] memory owners = _owners();
        bytes memory initializer = _setup(owners, handler);
        uint256 saltNonce = uint256(keccak256(abi.encode("safe-genesis-registry fork test", version)));

        // --- Gnosis: create the Safe, record its genesis --------------------
        vm.createSelectFork(gnosisRpc);
        SafeGenesisRegistry gnosisRegistry = new SafeGenesisRegistry();
        address safe = ISafeProxyFactory(factory).createProxyWithNonce(singleton, initializer, saltNonce);
        (address recorded, bool created) = gnosisRegistry.record(
            factory, ISafeProxyFactory(factory).proxyCreationCode(), singleton, initializer, saltNonce, "Fork Safe"
        );
        assertTrue(created);
        assertEq(recorded, safe, "the registry files it under the address the factory deployed");
        assertEq(ISafe(safe).VERSION(), version);

        // Owners drift after creation: the address no longer says who owns it.
        vm.prank(safe);
        ISafe(safe).swapOwner(owners[1], owners[2], makeAddr("owner-d"));

        // Read back by the Safe's own address.
        (bool found, SafeGenesisRegistry.Genesis memory g) = gnosisRegistry.genesisOf(safe);
        assertTrue(found);
        (address to, bytes memory data) = gnosisRegistry.deployCallOf(safe);

        // --- Base: deploy from the stored genesis ---------------------------
        vm.createSelectFork(baseRpc);
        assertEq(safe.code.length, 0, "the Safe must not exist on Base yet");
        assertEq(
            keccak256(ISafeProxyFactory(g.factory).proxyCreationCode()),
            g.proxyCreationCodeHash,
            "Base's factory has the same proxy code"
        );
        (bool ok, bytes memory ret) = to.call(data);
        assertTrue(ok);
        assertEq(abi.decode(ret, (address)), safe, "same address on Base");
        assertEq(ISafe(safe).VERSION(), version);
        assertEq(ISafe(safe).getOwners(), owners, "Base gets the ORIGINAL owners");
        assertEq(ISafe(safe).getThreshold(), 2);
    }

    function test_Fork_Safe130_CarriesAcrossChains() public {
        _carryAcrossChains(FACTORY_130, SAFE_L2_130, HANDLER_130, "1.3.0");
    }

    function test_Fork_Safe141_CarriesAcrossChains() public {
        _carryAcrossChains(FACTORY_141, SAFE_L2_141, HANDLER_141, "1.4.1");
    }
}
