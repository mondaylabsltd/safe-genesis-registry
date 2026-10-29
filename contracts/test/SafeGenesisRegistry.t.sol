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

    string internal constant NAME = "Team fund";

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
        return registry.record(address(factory), proxyCode, address(singleton), initializer, saltNonce, NAME);
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
            expected, address(factory), address(singleton), 7, keccak256(proxyCode), _init(), NAME
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
        (address otherSingleton,) = registry.record(address(factory), proxyCode, address(0xBEEF), _init(), 7, NAME);
        (address otherFactory,) = registry.record(address(0xFAC7), proxyCode, address(singleton), _init(), 7, NAME);
        (address otherCode,) = registry.record(address(factory), hex"6001", address(singleton), _init(), 7, NAME);

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
        registry.record(address(0), proxyCode, address(singleton), _init(), 7, NAME);
        vm.expectRevert(SafeGenesisRegistry.ZeroAddress.selector);
        registry.record(address(factory), proxyCode, address(0), _init(), 7, NAME);
    }

    function test_Record_RevertsOnEmptyInputs() public {
        vm.expectRevert(SafeGenesisRegistry.EmptyProxyCreationCode.selector);
        registry.record(address(factory), "", address(singleton), _init(), 7, NAME);
        vm.expectRevert(SafeGenesisRegistry.EmptyInitializer.selector);
        registry.record(address(factory), proxyCode, address(singleton), "", 7, NAME);
    }

    function test_Record_RevertsOnOversizedInitializer() public {
        uint256 tooLong = registry.MAX_INITIALIZER_LENGTH() + 1;
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.InitializerTooLong.selector, tooLong));
        registry.record(address(factory), proxyCode, address(singleton), new bytes(tooLong), 7, NAME);
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
    function testFuzz_RecordedAddressIsWhereTheFactoryDeploys(
        address[] memory owners,
        uint8 threshold,
        uint256 saltNonce
    ) public {
        vm.assume(owners.length > 0 && owners.length <= 20 && threshold > 0 && threshold <= owners.length);
        bytes memory initializer = _initializer(owners, threshold);
        (address recorded,) = _record(initializer, saltNonce);
        assertEq(factory.createProxyWithNonce(address(singleton), initializer, saltNonce), recorded);
        (address to, bytes memory data) = registry.deployCallOf(recorded);
        assertEq(to, address(factory));
        assertEq(
            data,
            abi.encodeCall(SafeLikeProxyFactory.createProxyWithNonce, (address(singleton), initializer, saltNonce))
        );
    }

    function test_AFullSizeRecordFitsOneTransaction() public {
        uint256 max = registry.MAX_INITIALIZER_LENGTH();
        bytes memory big = new bytes(max);
        for (uint256 i = 0; i < max; ++i) {
            big[i] = bytes1(uint8(i % 251) + 1); // no zero bytes: the costly case
        }
        uint256 before = gasleft();
        registry.record(address(factory), proxyCode, address(singleton), big, 1, NAME);
        uint256 used = before - gasleft();
        assertLt(used, 10_000_000, "well under the 16.78M per-transaction cap");
    }

    function testFuzz_RecordFilesUnderTheComputedAddress(address f, address s, bytes calldata init, uint256 n) public {
        vm.assume(f != address(0) && s != address(0) && init.length > 0 && init.length <= 1024);
        (address safe,) = registry.record(f, proxyCode, s, init, n, NAME);
        assertEq(safe, registry.computeAddress(f, proxyCode, s, init, n));
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertTrue(found);
        assertEq(g.saltNonce, n);
    }

    // -- names ------------------------------------------------------------------

    function _recordNamed(uint256 saltNonce, string memory name) internal returns (address safe, bool created) {
        return registry.record(address(factory), proxyCode, address(singleton), _init(), saltNonce, name);
    }

    function test_Name_IsALabelNotPartOfTheAddress() public {
        (address a,) = _recordNamed(7, "Team fund");
        assertEq(a, _expected(_init(), 7), "the address ignores the name");
        assertEq(_replay(a), a);
        (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(a);
        assertTrue(found);
        assertEq(g.name, "Team fund");
        assertEq(g.saltNonce, 7);
    }

    function test_Name_MayRepeat() public {
        (address a,) = _recordNamed(7, "Team fund");
        (address b, bool created) = _recordNamed(8, "Team fund");
        assertTrue(created);
        assertTrue(a != b);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(b);
        assertEq(g.name, "Team fund");
    }

    function test_Name_TheFirstWriteKeepsIt() public {
        _recordNamed(7, "Team fund");
        vm.prank(dave);
        (address safe, bool created) = _recordNamed(7, "Something else");
        assertFalse(created);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Team fund", "never changed");
        assertEq(g.recorder, address(this));
        assertEq(registry.totalRecords(), 1);
    }

    /// A name is required, 1 to 64 bytes; a bad one reverts even when the
    /// Safe is recorded already.
    function test_Name_RevertsWhenEmptyOrTooLong() public {
        vm.expectRevert(SafeGenesisRegistry.EmptyName.selector);
        _recordNamed(7, "");
        string memory long = string(new bytes(65));
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.NameTooLong.selector, 65));
        _recordNamed(7, long);
        _recordNamed(7, string(new bytes(64)));
        vm.expectRevert(SafeGenesisRegistry.EmptyName.selector);
        _recordNamed(7, "");
    }

    function testFuzz_Name_FilesWhereTheFactoryDeploys(uint256 saltNonce, string calldata name) public {
        vm.assume(bytes(name).length > 0 && bytes(name).length <= 64);
        (address safe,) = _recordNamed(saltNonce, name);
        assertEq(safe, _expected(_init(), saltNonce));
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, name);
    }

    // -- rename ---------------------------------------------------------------------

    function test_Rename_TheSafeRenamesItself() public {
        (address safe,) = _recordNamed(7, "Squatted");
        vm.roll(500);
        vm.expectEmit(true, false, false, true, address(registry));
        emit SafeGenesisRegistry.GenesisRenamed(safe, "Family vault");
        vm.prank(safe);
        registry.rename("Family vault");
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Family vault");
        assertEq(g.renamedAt, 500);
        (,, SafeGenesisRegistry.Genesis[] memory page) = registry.genesisPage(0, 1, true);
        assertEq(page[0].name, "Family vault");
        assertEq(page[0].renamedAt, 500);
    }

    /// A rename moves the label only: the genesis, the replay and the
    /// record's place stay as they were.
    function test_Rename_ChangesOnlyTheName() public {
        vm.prank(dave);
        (address safe,) = _recordNamed(7, "First");
        (, SafeGenesisRegistry.Genesis memory before) = registry.genesisOf(safe);
        (address toBefore, bytes memory dataBefore) = registry.deployCallOf(safe);
        vm.roll(block.number + 10);
        vm.prank(safe);
        registry.rename("Second");
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        before.name = "Second";
        before.renamedAt = uint64(block.number);
        assertEq(abi.encode(g), abi.encode(before));
        (address to, bytes memory data) = registry.deployCallOf(safe);
        assertEq(to, toBefore);
        assertEq(data, dataBefore);
        assertEq(registry.totalRecords(), 1);
        assertEq(_replay(safe), safe);
    }

    function test_Rename_AnyNumberOfTimes() public {
        (address safe,) = _recordNamed(7, "One");
        vm.startPrank(safe);
        registry.rename("Two");
        registry.rename("Three");
        vm.stopPrank();
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Three");
    }

    /// Only the Safe: the recorder, an owner or anyone else renames nothing,
    /// because their own address holds no record.
    function test_Rename_OnlyTheSafe() public {
        vm.prank(dave);
        (address safe,) = _recordNamed(7, "Kept");
        address[3] memory others = [dave, alice, address(this)];
        for (uint256 i; i < 3; ++i) {
            vm.prank(others[i]);
            vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.NotRecorded.selector, others[i]));
            registry.rename("Taken");
        }
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Kept");
        assertEq(g.renamedAt, 0);
    }

    function test_Rename_RevertsOnEmptyOrLongNames() public {
        (address safe,) = _recordNamed(7, "Kept");
        vm.startPrank(safe);
        vm.expectRevert(SafeGenesisRegistry.EmptyName.selector);
        registry.rename("");
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.NameTooLong.selector, 65));
        registry.rename(string(new bytes(65)));
        registry.rename(string(new bytes(64)));
        vm.stopPrank();
    }

    /// A repeat write after a rename keeps the Safe's own name.
    function test_Rename_ARepeatWriteKeepsTheSafesName() public {
        (address safe,) = _recordNamed(7, "Squatted");
        vm.prank(safe);
        registry.rename("Mine");
        (, bool created) = _recordNamed(7, "Squatted again");
        assertFalse(created);
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "Mine");
    }

    /// The deployed Safe renames itself through its own call.
    function test_Rename_FromTheDeployedSafe() public {
        (address safe,) = _recordNamed(7, "Squatted");
        _replay(safe);
        MockSafeSingleton(safe).call(address(registry), abi.encodeCall(SafeGenesisRegistry.rename, ("From the Safe")));
        (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safe);
        assertEq(g.name, "From the Safe");
    }

    function testFuzz_Rename_KeepsEnumerationIntact(uint8 n, uint8 who, string calldata name) public {
        vm.assume(bytes(name).length > 0 && bytes(name).length <= 64);
        n = uint8(bound(n, 1, 12));
        address[] memory safes = _recordMany(n);
        uint256 i = bound(who, 0, n - 1);
        vm.prank(safes[i]);
        registry.rename(name);
        for (uint256 k; k < n; ++k) {
            assertEq(registry.safeAt(k), safes[k]);
            (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safes[k]);
            assertEq(g.index, k);
            if (k == i) assertEq(g.name, name);
            else assertEq(g.renamedAt, 0);
        }
    }

    // -- enumeration ------------------------------------------------------------

    /// `n` Safes, recorded with salt nonces 0..n-1; the even ones named.
    function _recordMany(uint256 n) internal returns (address[] memory safes) {
        safes = new address[](n);
        for (uint256 i; i < n; ++i) {
            (safes[i],) = i % 2 == 0 ? _recordNamed(i, "Even") : _record(_init(), i);
        }
    }

    function _page(uint256 offset, uint256 limit, bool desc)
        internal
        view
        returns (uint256 total, address[] memory safes, SafeGenesisRegistry.Genesis[] memory records)
    {
        return registry.genesisPage(offset, limit, desc);
    }

    function test_Enumeration_FollowsWriteOrder() public {
        address[] memory safes = _recordMany(3);
        assertEq(registry.totalRecords(), 3);
        for (uint256 i; i < 3; ++i) {
            assertEq(registry.safeAt(i), safes[i]);
            (, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safes[i]);
            assertEq(g.index, i);
        }
    }

    function test_Enumeration_ARepeatWriteAddsNothing() public {
        address[] memory safes = _recordMany(2);
        _record(_init(), 0);
        _recordNamed(1, "Late name");
        assertEq(registry.totalRecords(), 2);
        assertEq(registry.safeAt(1), safes[1]);
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.IndexOutOfBounds.selector, 2, 2));
        registry.safeAt(2);
    }

    function test_Enumeration_EmptyRegistry() public {
        (uint256 total, address[] memory safes, SafeGenesisRegistry.Genesis[] memory records) = _page(0, 20, true);
        assertEq(total, 0);
        assertEq(safes.length, 0);
        assertEq(records.length, 0);
        vm.expectRevert(abi.encodeWithSelector(SafeGenesisRegistry.IndexOutOfBounds.selector, 0, 0));
        registry.safeAt(0);
    }

    function test_GenesisPage_OldestAndNewestFirst() public {
        address[] memory all = _recordMany(5);

        (uint256 total, address[] memory safes, SafeGenesisRegistry.Genesis[] memory records) = _page(1, 2, false);
        assertEq(total, 5);
        assertEq(safes.length, 2);
        assertEq(safes[0], all[1]);
        assertEq(safes[1], all[2]);
        assertEq(records[0].index, 1);
        assertEq(records[1].name, "Even");

        (, safes, records) = _page(1, 2, true);
        assertEq(safes[0], all[3]);
        assertEq(safes[1], all[2]);
        assertEq(records[0].index, 3);

        // The last page is short; past the end is empty.
        (, safes,) = _page(4, 20, true);
        assertEq(safes.length, 1);
        assertEq(safes[0], all[0]);
        (, safes,) = _page(5, 20, false);
        assertEq(safes.length, 0);
        (, safes,) = _page(0, 0, false);
        assertEq(safes.length, 0);
    }

    /// Extreme arguments neither overflow nor revert.
    function test_GenesisPage_ExtremeArguments() public {
        _recordMany(3);
        (uint256 total, address[] memory safes,) = _page(type(uint256).max, type(uint256).max, true);
        assertEq(total, 3);
        assertEq(safes.length, 0);
        (, safes,) = _page(0, type(uint256).max, true);
        assertEq(safes.length, 3);
        (, safes,) = _page(2, type(uint256).max, false);
        assertEq(safes.length, 1);
    }

    /// A page carries each record exactly as `genesisOf` returns it.
    function test_GenesisPage_RecordsMatchGenesisOf() public {
        vm.roll(1234);
        vm.startPrank(dave);
        _recordMany(4);
        vm.stopPrank();
        (, address[] memory safes, SafeGenesisRegistry.Genesis[] memory records) = _page(0, 4, true);
        for (uint256 i; i < 4; ++i) {
            (bool found, SafeGenesisRegistry.Genesis memory g) = registry.genesisOf(safes[i]);
            assertTrue(found);
            assertEq(abi.encode(records[i]), abi.encode(g));
        }
        assertEq(records[0].recorder, dave);
        assertEq(records[0].recordedAt, 1234);
    }

    function testFuzz_GenesisPage_AgreesWithSafeAt(uint8 n, uint256 offset, uint256 limit, bool desc) public {
        n = uint8(bound(n, 0, 24));
        offset = bound(offset, 0, 30);
        limit = bound(limit, 0, 30);
        _recordMany(n);

        (uint256 total, address[] memory safes, SafeGenesisRegistry.Genesis[] memory records) =
            _page(offset, limit, desc);
        assertEq(total, n);
        uint256 expected = offset >= n ? 0 : (n - offset < limit ? n - offset : limit);
        assertEq(safes.length, expected);
        assertEq(records.length, expected);
        for (uint256 i; i < expected; ++i) {
            uint256 index = desc ? n - 1 - offset - i : offset + i;
            assertEq(safes[i], registry.safeAt(index));
            assertEq(records[i].index, index);
        }
    }

    /// What a 100-record page costs an `eth_call`, storage cold, with this
    /// suite's ~420-byte initializers: about 5.7M, within common gas caps.
    function test_GenesisPage_HundredRecordsGas() public {
        _recordMany(100);
        vm.cool(address(registry));
        uint256 before = gasleft();
        (, address[] memory safes,) = _page(0, 100, true);
        uint256 used = before - gasleft();
        emit log_named_uint("gas for a 100-record page", used);
        assertEq(safes.length, 100);
        assertLt(used, 8_000_000);
    }
}
