// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {SafeGenesisRegistry} from "../src/SafeGenesisRegistry.sol";
import {
    SafeLikeProxyFactory,
    MockSafeSingleton,
    CountingCallback,
    IProxyCreationCallback
} from "./fixtures/SafeLike.sol";

contract SafeGenesisRegistryTest is Test {
    SafeGenesisRegistry internal registry;
    SafeLikeProxyFactory internal factory;
    MockSafeSingleton internal singleton;
    bytes internal proxyCode;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal dave = makeAddr("dave");

    function setUp() public {
        registry = new SafeGenesisRegistry();
        factory = new SafeLikeProxyFactory();
        singleton = new MockSafeSingleton();
        proxyCode = factory.proxyCreationCode();
    }

    // -- helpers ------------------------------------------------------------

    function _initializer(address[] memory owners, uint256 threshold) internal pure returns (bytes memory) {
        return abi.encodeCall(
            MockSafeSingleton.setup, (owners, threshold, address(0), "", address(0), address(0), 0, payable(address(0)))
        );
    }

    function _owners3() internal view returns (address[] memory owners) {
        owners = new address[](3);
        owners[0] = alice;
        owners[1] = bob;
        owners[2] = carol;
    }

    function _init() internal view returns (bytes memory) {
        return _initializer(_owners3(), 2);
    }

    /// The address the factory itself deploys to — computed independently of
    /// the registry, with forge's CREATE2 helper.
    function _expected(bytes memory initializer, uint256 saltNonce) internal view returns (address) {
        bytes32 salt = keccak256(abi.encodePacked(keccak256(initializer), saltNonce));
        bytes32 initCodeHash = keccak256(abi.encodePacked(proxyCode, uint256(uint160(address(singleton)))));
        return vm.computeCreate2Address(salt, initCodeHash, address(factory));
    }

    function _record(bytes memory initializer, uint256 saltNonce) internal returns (address safe, bool created) {
        return registry.record(address(factory), proxyCode, address(singleton), initializer, saltNonce);
    }

    function _replay(address safe) internal returns (address deployed) {
        (address to, bytes memory data) = registry.deployCallOf(safe);
        (bool ok, bytes memory ret) = to.call(data);
        assertTrue(ok, "replay failed");
        deployed = abi.decode(ret, (address));
    }

    // -- record ---------------------------------------------------------------

    function test_Record_FilesUnderTheAddressTheFactoryDeploysTo() public {
        address expected = _expected(_init(), 7);

        vm.expectEmit(true, true, true, true, address(registry));
        emit SafeGenesisRegistry.GenesisRecorded(
            expected, address(factory), address(singleton), 7, keccak256(proxyCode), _init()
        );
        vm.prank(dave);
        (address safe, bool created) = _record(_init(), 7);

        assertEq(safe, expected);
        assertTrue(created);
        assertEq(safe.code.length, 0, "recording deploys nothing");
        assertEq(factory.createProxyWithNonce(address(singleton), _init(), 7), safe, "the factory agrees");

        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertTrue(found);
        assertEq(g.factory, address(factory));
        assertEq(g.singleton, address(singleton));
        assertEq(g.saltNonce, 7);
        assertEq(g.initializer, _init());
        assertEq(g.proxyCreationCodeHash, keccak256(proxyCode));
        assertEq(g.recorder, dave);
        assertEq(g.recordedAt, block.number);
        assertTrue(registry.isRecorded(safe));
        assertEq(registry.totalRecords(), 1);
    }

    /// The whole point: owners change on chain A, yet the stored genesis
    /// deploys the Safe at the SAME address on chain B, with the ORIGINAL
    /// owners (the wallet then syncs them).
    function test_GenesisReplaysToSameAddressOnAnotherChain() public {
        uint256 beforeChainA = vm.snapshotState();

        vm.chainId(100);
        address safe = factory.createProxyWithNonce(address(singleton), _init(), 7);
        _record(_init(), 7);
        MockSafeSingleton(safe).swapOwner(2, dave); // owners drift on chain A
        (address to, bytes memory data) = registry.deployCallOf(safe);

        // A fresh chain: nothing deployed, nothing recorded.
        vm.revertToState(beforeChainA);
        vm.chainId(8453);
        assertEq(safe.code.length, 0);
        assertFalse(registry.isRecorded(safe));

        (bool ok, bytes memory ret) = to.call(data);
        assertTrue(ok);
        assertEq(abi.decode(ret, (address)), safe);
        assertEq(MockSafeSingleton(safe).getOwners(), _owners3());
    }

    function test_Record_IsWriteOnce() public {
        (address a, bool createdA) = _record(_init(), 7);
        vm.prank(dave);
        (address b, bool createdB) = _record(_init(), 7);
        assertEq(a, b);
        assertTrue(createdA);
        assertFalse(createdB);
        assertEq(registry.totalRecords(), 1);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(a);
        assertEq(g.recorder, address(this), "the first writer stays");
    }

    function test_Record_WorksBeforeAndAfterDeployment() public {
        (address before,) = _record(_init(), 7);
        assertEq(_replay(before), before);

        address deployed = factory.createProxyWithNonce(address(singleton), _init(), 8);
        (address after_, bool created) = _record(_init(), 8);
        assertEq(after_, deployed);
        assertTrue(created);
    }

    /// Any change to any parameter is a different Safe: nothing but a Safe's
    /// own genesis can be filed under its address.
    function test_AnyOtherParameterIsADifferentSafe() public {
        (address real,) = _record(_init(), 7);

        (address otherOwners,) = _record(_initializer(_owners3(), 1), 7);
        (address otherNonce,) = _record(_init(), 8);
        (address otherSingleton,) = registry.record(address(factory), proxyCode, address(0xBEEF), _init(), 7);
        (address otherFactory,) = registry.record(address(0xFAC7), proxyCode, address(singleton), _init(), 7);
        (address otherCode,) = registry.record(address(factory), hex"6001", address(singleton), _init(), 7);

        assertTrue(otherOwners != real);
        assertTrue(otherNonce != real);
        assertTrue(otherSingleton != real);
        assertTrue(otherFactory != real);
        assertTrue(otherCode != real);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(real);
        assertEq(g.initializer, _init(), "the real record is untouched");
    }

    /// A Safe made with `createProxyWithCallback` is the nonce-form Safe with
    /// the callback folded into the salt nonce: recorded that way, it replays
    /// to the same address and setup (without calling the callback).
    function test_CallbackSafeRecordsInNonceForm() public {
        CountingCallback callback = new CountingCallback();
        uint256 beforeChainA = vm.snapshotState();

        address safe =
            factory.createProxyWithCallback(address(singleton), _init(), 7, IProxyCreationCallback(address(callback)));
        assertEq(callback.calls(), 1);
        uint256 folded = uint256(keccak256(abi.encodePacked(uint256(7), address(callback))));
        (address recorded,) = _record(_init(), folded);
        assertEq(recorded, safe);
        (address to, bytes memory data) = registry.deployCallOf(safe);

        vm.revertToState(beforeChainA); // another chain: the Safe does not exist yet
        (bool ok, bytes memory ret) = to.call(data);
        assertTrue(ok);
        assertEq(abi.decode(ret, (address)), safe);
        assertEq(MockSafeSingleton(safe).getOwners(), _owners3());
        assertEq(callback.calls(), 0, "the replay does not call the callback");
    }

    function test_Record_RevertsOnZeroAddresses() public {
        vm.expectRevert(SafeGenesisRegistry.ZeroAddress.selector);
        registry.record(address(0), proxyCode, address(singleton), _init(), 7);
        vm.expectRevert(SafeGenesisRegistry.ZeroAddress.selector);
        registry.record(address(factory), proxyCode, address(0), _init(), 7);
    }

    function test_Record_RevertsOnEmptyInputs() public {
        vm.expectRevert(SafeGenesisRegistry.EmptyProxyCreationCode.selector);
        registry.record(address(factory), "", address(singleton), _init(), 7);
        vm.expectRevert(SafeGenesisRegistry.EmptyInitializer.selector);
        registry.record(address(factory), proxyCode, address(singleton), "", 7);
    }

    function test_Record_RevertsOnOversizedInitializer() public {
        uint256 tooLong = registry.MAX_INITIALIZER_LENGTH() + 1;
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.InitializerTooLong.selector, tooLong));
        registry.record(address(factory), proxyCode, address(singleton), new bytes(tooLong), 7);
    }

    // -- reads ------------------------------------------------------------------

    function test_UnrecordedSafeReadsEmpty() public view {
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(alice);
        assertFalse(found);
        assertEq(g.factory, address(0));
        assertFalse(registry.isRecorded(alice));
        (address to, bytes memory data) = registry.deployCallOf(alice);
        assertEq(to, address(0));
        assertEq(data.length, 0);
    }

    function test_DeployCallOfIsTheCanonicalFactoryCall() public {
        (address safe,) = _record(_init(), 7);
        (address to, bytes memory data) = registry.deployCallOf(safe);
        assertEq(to, address(factory));
        assertEq(data, abi.encodeCall(SafeLikeProxyFactory.createProxyWithNonce, (address(singleton), _init(), 7)));
    }

    /// Against the factory itself, not a restatement of the formula: the
    /// address the registry files a record under is the address the factory
    /// then deploys to.
    function testFuzz_RecordedAddressIsWhereTheFactoryDeploys(address[] memory owners, uint8 threshold, uint256 saltNonce)
        public
    {
        vm.assume(owners.length > 0 && owners.length <= 20 && threshold > 0 && threshold <= owners.length);
        bytes memory initializer = _initializer(owners, threshold);
        (address recorded,) = _record(initializer, saltNonce);
        assertEq(factory.createProxyWithNonce(address(singleton), initializer, saltNonce), recorded);
        (address to, bytes memory data) = registry.deployCallOf(recorded);
        assertEq(to, address(factory));
        assertEq(data, abi.encodeCall(SafeLikeProxyFactory.createProxyWithNonce, (address(singleton), initializer, saltNonce)));
    }

    function test_AFullSizeRecordFitsOneTransaction() public {
        uint256 max = registry.MAX_INITIALIZER_LENGTH();
        bytes memory big = new bytes(max);
        for (uint256 i = 0; i < max; ++i) {
            big[i] = bytes1(uint8(i % 251) + 1); // no zero bytes: the costly case
        }
        uint256 before = gasleft();
        registry.record(address(factory), proxyCode, address(singleton), big, 1);
        uint256 used = before - gasleft();
        assertLt(used, 10_000_000, "well under the 16.78M per-transaction cap");
    }

    function testFuzz_RecordFilesUnderTheComputedAddress(address f, address s, bytes calldata init, uint256 n) public {
        vm.assume(f != address(0) && s != address(0) && init.length > 0 && init.length <= 1024);
        (address safe,) = registry.record(f, proxyCode, s, init, n);
        assertEq(safe, registry.computeAddress(f, proxyCode, s, init, n));
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertTrue(found);
        assertEq(g.saltNonce, n);
    }

    // -- recordNamed ------------------------------------------------------------

    bytes32 internal constant SEED = keccak256("seed");

    function _recordNamed(string memory name, bytes32 seed) internal returns (address safe, bool created) {
        return registry.recordNamed(address(factory), proxyCode, address(singleton), _init(), name, seed);
    }

    function test_RecordNamed_TheNameIsPartOfTheAddress() public {
        (address safe, bool created) = _recordNamed("Team fund", SEED);
        assertTrue(created);
        uint256 saltNonce = registry.namedSaltNonce("Team fund", SEED);
        assertEq(saltNonce, uint256(keccak256(abi.encode(keccak256("SafeGenesisRegistry.name"), keccak256("Team fund"), SEED))));
        assertEq(safe, _expected(_init(), saltNonce), "where the factory deploys it");
        assertEq(_replay(safe), safe);
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertTrue(found);
        assertEq(g.name, "Team fund");
        assertEq(g.nameSeed, SEED);
        assertEq(g.saltNonce, saltNonce);
    }

    function test_RecordNamed_AnotherNameIsAnotherSafe() public {
        (address a,) = _recordNamed("Team fund", SEED);
        (address b, bool created) = _recordNamed("Team fund!", SEED);
        assertTrue(created);
        assertTrue(a != b);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(a);
        assertEq(g.name, "Team fund", "the first is untouched");
    }

    function test_RecordNamed_IsWriteOnce() public {
        (address a,) = _recordNamed("Team fund", SEED);
        vm.prank(dave);
        (address b, bool created) = _recordNamed("Team fund", SEED);
        assertEq(a, b);
        assertFalse(created);
        assertEq(registry.totalRecords(), 1);
    }

    /// Someone copies the salt nonce out of a pending recordNamed and records
    /// it first without the name: the named call still names it.
    function test_RecordNamed_NamesASafeRecordedFirstWithoutIt() public {
        uint256 saltNonce = registry.namedSaltNonce("Team fund", SEED);
        vm.prank(dave);
        (address copied,) = _record(_init(), saltNonce);
        (, SafeGenesisRegistry.Genesis memory before) = registry.genesisOf(copied);
        assertEq(before.name, "");
        (address safe, bool created) = _recordNamed("Team fund", SEED);
        assertEq(safe, copied);
        assertFalse(created);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Team fund");
        assertEq(g.recorder, dave, "the first writer stays");
        assertEq(registry.totalRecords(), 1);
    }

    function test_RecordNamed_RevertsOnEmptyOrLongNames() public {
        vm.expectRevert(SafeGenesisRegistry.EmptyName.selector);
        _recordNamed("", SEED);
        string memory long = string(new bytes(65));
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.NameTooLong.selector, 65));
        _recordNamed(long, SEED);
        _recordNamed(string(new bytes(64)), SEED);
    }

    function test_Record_KeepsNoName() public {
        (address safe,) = _record(_init(), 7);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "");
        assertEq(g.nameSeed, bytes32(0));
    }

    function testFuzz_RecordNamed_FilesWhereTheFactoryDeploys(string calldata name, bytes32 seed) public {
        vm.assume(bytes(name).length > 0 && bytes(name).length <= 64);
        (address safe,) = _recordNamed(name, seed);
        assertEq(safe, _expected(_init(), registry.namedSaltNonce(name, seed)));
    }
}
