// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @title SafeGenesisRegistry
/// @author Built by Vela Wallet (https://getvela.app)
/// @notice A neutral, permissionless, write-once registry of the GENESIS of
///         Safe accounts: for a Safe address, the creation parameters that
///         deploy it. A Safe's address is fixed by its creation call forever,
///         while its owners may change, so the same Safe can only be deployed
///         on a new chain by replaying the ORIGINAL creation call — which
///         nothing on chain keeps in readable form. This contract keeps it,
///         readable by plain `eth_call`, one record per Safe.
///
///         The registry derives the address itself, exactly as Safe's proxy
///         factory does in `createProxyWithNonce` (unchanged from v1.1.1
///         through v1.4.1):
///
///           salt         = keccak256(keccak256(initializer) ‖ uint256(saltNonce))
///           initCodeHash = keccak256(proxyCreationCode ‖ uint256(uint160(singleton)))
///           safe         = address(keccak256(0xff ‖ factory ‖ salt ‖ initCodeHash)[12:])
///
///         Nothing in a record is taken on trust: every field is an input to
///         the address. A different singleton, initializer, salt nonce,
///         factory or proxy code gives a different address, so filing other
///         parameters under an existing Safe's address takes a 160-bit
///         address preimage. Hence one record per Safe, written once, by
///         anyone, and never changed.
///
///         What a record does NOT prove — the reader's checks, before a replay:
///         - that `factory` is Safe's factory: a hostile contract at that
///           address on another chain can return the same proxy code and
///           deploy the same address with its own setup. Check the runtime
///           code hash of the factory, the singleton and every contract the
///           initializer reaches, and the deployed Safe's configuration after.
///         - for a Safe nobody has deployed yet: a party that chose BOTH
///           parameter sets could, with a ~2^80 birthday search, make two
///           creations share one address. Fund an undeployed Safe only on
///           parameters you generated.
///
///         Scope, stated plainly:
///         - Any Safe version whose factory derives the salt this way:
///           factory, proxy creation code and singleton are parameters, not
///           constants. A Safe made with `createProxyWithCallback` (v1.1.1 –
///           v1.4.1) is the same Safe as `createProxyWithNonce` with
///           `saltNonce = uint256(keccak256(saltNonce ‖ callback))`; record it
///           in that form (a replay deploys the same Safe, without calling
///           the callback). v1.5.0's `createProxyWithNonceL2` uses the same
///           salt; `deployCallOf` returns the plain form, which skips its
///           extra event.
///         - Not `createChainSpecificProxyWithNonce`: its address differs on
///           every chain, so there is nothing to carry across.
///         - A record says nothing about a Safe's CURRENT owners: a Safe
///           redeployed from its genesis starts with its original owners.
///         - `totalRecords`, `isRecorded` and the enumeration (`safeAt`,
///           `genesisPage`) cover any record, including ones filed at
///           addresses no Safe lives at.
///         - Every record has a NAME: a label, not an input to the address.
///           Whoever writes the record first chooses it, so on its own it
///           proves nothing, and names may repeat. The Safe itself can
///           replace it at any time (`rename`, called by the Safe, so by its
///           owners' transaction), on a chain where it is deployed. A name
///           is the Safe's own when `renamedAt` is not zero, or when the Safe
///           wrote the record itself (`recorder` is the Safe). A rename moves
///           the label only: the genesis never changes. Each deployment of
///           the registry keeps its own names. The address is the Safe's
///           identity.
///         - The registry makes no external call.
///         - EVM CREATE2 only: chains that derive CREATE2 addresses
///           differently (zkSync Era) are out of scope.
contract SafeGenesisRegistry {
    uint8 public constant VERSION = 4;

    /// Upper bound on a stored initializer. A Safe `setup` with dozens of
    /// owners fits in a few KiB. The bound keeps a record writable in one
    /// transaction on every chain: a full 8 KiB record costs about 7M gas,
    /// well under the 16.78M per-transaction cap (EIP-7825).
    uint256 public constant MAX_INITIALIZER_LENGTH = 8_192;

    /// Upper bound on a name, in bytes (UTF-8 is not checked on chain).
    uint256 public constant MAX_NAME_LENGTH = 64;

    /// `createProxyWithNonce(address,bytes,uint256)`.
    bytes4 public constant CREATE_PROXY_WITH_NONCE = 0x1688f0b9;

    struct Genesis {
        /// The proxy factory that deploys the Safe.
        address factory;
        /// The Safe singleton (mastercopy) the proxy delegates to.
        address singleton;
        /// Block number of the Safe's last `rename`, 0 if it never renamed
        /// itself: then `name` is the first writer's. Shares the storage slot
        /// of `singleton`.
        uint64 renamedAt;
        /// The factory's salt nonce.
        uint256 saltNonce;
        /// The `setup(...)` call the factory makes on the new proxy: the
        /// original owners, threshold, fallback handler and setup hook.
        bytes initializer;
        /// keccak256 of the factory's proxy creation code. A replay must use
        /// a factory whose `proxyCreationCode()` hashes to this.
        bytes32 proxyCreationCodeHash;
        /// Who wrote the record. Informational only.
        address recorder;
        /// Block number of the write. Informational only.
        uint64 recordedAt;
        /// Position in write order, from 0: `safeAt(index)` is this Safe.
        /// Shares the storage slot of `recorder` and `recordedAt`.
        uint32 index;
        /// The name, 1 to 64 bytes: the first writer's, or the Safe's own
        /// after a `rename` (or when the Safe is the `recorder`). A label:
        /// not part of the address, not unique.
        string name;
    }

    mapping(address safe => Genesis) private _genesis;

    /// Every recorded Safe, in write order. Append-only.
    address[] private _safes;

    event GenesisRecorded(
        address indexed safe,
        address indexed factory,
        address indexed singleton,
        uint256 saltNonce,
        bytes32 proxyCreationCodeHash,
        bytes initializer,
        string name
    );

    event GenesisRenamed(address indexed safe, string name);

    error ZeroAddress();
    error EmptyName();
    error NameTooLong(uint256 length);
    error EmptyProxyCreationCode();
    error EmptyInitializer();
    error InitializerTooLong(uint256 length);
    error IndexOutOfBounds(uint256 index, uint256 total);
    error RegistryFull();
    error NotRecorded(address safe);

    // -----------------------------------------------------------------------
    // Write
    // -----------------------------------------------------------------------

    /// File the genesis of the Safe these parameters create, with a name,
    /// under the address they create it at. Returns that address; a caller
    /// that expects a specific Safe compares it. Write-once and idempotent:
    /// the same parameters again return `created = false` and keep the first
    /// write's name, and no other parameters can reach the same address.
    ///
    /// `proxyCreationCode` is the factory's `proxyCreationCode()`. It is
    /// hashed, not stored: a wrong one only moves the record to an address
    /// no Safe lives at. `name` is not part of the address.
    function record(
        address factory,
        bytes calldata proxyCreationCode,
        address singleton,
        bytes calldata initializer,
        uint256 saltNonce,
        string calldata name
    ) external returns (address safe, bool created) {
        _check(factory, singleton, proxyCreationCode.length, initializer.length, bytes(name).length);
        safe = _address(factory, proxyCreationCode, singleton, initializer, saltNonce);
        if (_genesis[safe].factory != address(0)) return (safe, false);
        _store(safe, factory, singleton, saltNonce, keccak256(proxyCreationCode));
        _storeBytes(safe, initializer, name);
        return (safe, true);
    }

    /// Rename the calling Safe: `msg.sender` is the Safe, so the call comes
    /// from its own transaction, approved by its owners. Only the name and
    /// `renamedAt` change; a Safe may rename itself any number of times.
    /// On a chain where the Safe is not deployed nothing can make this call.
    function rename(string calldata name) external {
        Genesis storage g = _genesis[msg.sender];
        if (g.factory == address(0)) revert NotRecorded(msg.sender);
        uint256 length = bytes(name).length;
        if (length == 0) revert EmptyName();
        if (length > MAX_NAME_LENGTH) revert NameTooLong(length);
        g.name = name;
        g.renamedAt = uint64(block.number);
        emit GenesisRenamed(msg.sender, name);
    }

    // -----------------------------------------------------------------------
    // Reads — by the Safe's own address
    // -----------------------------------------------------------------------

    /// The Safe's genesis, if recorded.
    function genesisOf(address safe) external view returns (bool found, Genesis memory genesis) {
        genesis = _genesis[safe];
        found = genesis.factory != address(0);
    }

    function isRecorded(address safe) external view returns (bool) {
        return _genesis[safe].factory != address(0);
    }

    /// The call that deploys the Safe on another chain: send `data` to
    /// `factory`. Both empty when the Safe is not recorded.
    function deployCallOf(address safe) external view returns (address factory, bytes memory data) {
        Genesis storage g = _genesis[safe];
        if (g.factory == address(0)) return (address(0), "");
        return (g.factory, abi.encodeWithSelector(CREATE_PROXY_WITH_NONCE, g.singleton, g.initializer, g.saltNonce));
    }

    // -----------------------------------------------------------------------
    // Reads — in write order
    // -----------------------------------------------------------------------

    /// How many records there are; their indexes run from 0 to this minus one.
    function totalRecords() external view returns (uint256) {
        return _safes.length;
    }

    /// The Safe recorded at `index` (0 = the first record).
    function safeAt(uint256 index) external view returns (address) {
        uint256 total = _safes.length;
        if (index >= total) revert IndexOutOfBounds(index, total);
        return _safes[index];
    }

    /// Up to `limit` complete records, skipping the first `offset` in the
    /// chosen order: oldest first, or newest first when `desc`. `safes[i]` is
    /// the Safe of `records[i]`; `total` is `totalRecords()` at the time of
    /// the read, so a reader paging newest-first can see that records were
    /// added since its last page. Past the end, both arrays are empty.
    ///
    /// The cost grows with the page and with each record's initializer; a
    /// reader keeps `limit` within its node's `eth_call` gas cap.
    function genesisPage(uint256 offset, uint256 limit, bool desc)
        external
        view
        returns (uint256 total, address[] memory safes, Genesis[] memory records)
    {
        total = _safes.length;
        uint256 count = offset < total ? total - offset : 0;
        if (limit < count) count = limit;

        safes = new address[](count);
        records = new Genesis[](count);
        for (uint256 i; i < count; ++i) {
            address safe = _safes[desc ? total - 1 - offset - i : offset + i];
            safes[i] = safe;
            records[i] = _genesis[safe];
        }
    }

    // -----------------------------------------------------------------------
    // Pure
    // -----------------------------------------------------------------------

    /// The address `createProxyWithNonce(singleton, initializer, saltNonce)`
    /// on `factory` deploys, given that factory's proxy creation code.
    function computeAddress(
        address factory,
        bytes calldata proxyCreationCode,
        address singleton,
        bytes calldata initializer,
        uint256 saltNonce
    ) external pure returns (address) {
        return _address(factory, proxyCreationCode, singleton, initializer, saltNonce);
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    function _check(
        address factory,
        address singleton,
        uint256 proxyCreationCodeLength,
        uint256 initializerLength,
        uint256 nameLength
    ) private pure {
        if (factory == address(0) || singleton == address(0)) revert ZeroAddress();
        if (proxyCreationCodeLength == 0) revert EmptyProxyCreationCode();
        if (initializerLength == 0) revert EmptyInitializer();
        if (initializerLength > MAX_INITIALIZER_LENGTH) revert InitializerTooLong(initializerLength);
        if (nameLength == 0) revert EmptyName();
        if (nameLength > MAX_NAME_LENGTH) revert NameTooLong(nameLength);
    }

    function _store(address safe, address factory, address singleton, uint256 saltNonce, bytes32 proxyCreationCodeHash)
        private
    {
        uint256 index = _safes.length;
        // 2^32 records would take more gas than any chain has produced; the
        // cast still never truncates silently.
        if (index > type(uint32).max) revert RegistryFull();

        Genesis storage g = _genesis[safe];
        g.factory = factory;
        g.singleton = singleton;
        g.saltNonce = saltNonce;
        g.proxyCreationCodeHash = proxyCreationCodeHash;
        g.recorder = msg.sender;
        g.recordedAt = uint64(block.number);
        g.index = uint32(index);
        _safes.push(safe);
    }

    /// The dynamic fields, and the event: split from `_store` to stay within
    /// the stack. The fixed fields come back from slots written just now.
    function _storeBytes(address safe, bytes calldata initializer, string calldata name) private {
        Genesis storage g = _genesis[safe];
        g.initializer = initializer;
        g.name = name;
        emit GenesisRecorded(safe, g.factory, g.singleton, g.saltNonce, g.proxyCreationCodeHash, initializer, name);
    }

    function _address(
        address factory,
        bytes calldata proxyCreationCode,
        address singleton,
        bytes calldata initializer,
        uint256 saltNonce
    ) private pure returns (address) {
        bytes32 salt = keccak256(abi.encodePacked(keccak256(initializer), saltNonce));
        bytes32 initCodeHash = keccak256(abi.encodePacked(proxyCreationCode, uint256(uint160(singleton))));
        return address(uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), factory, salt, initCodeHash)))));
    }
}
