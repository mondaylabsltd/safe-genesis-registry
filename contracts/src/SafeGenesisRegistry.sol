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
///         - `totalRecords` and `isRecorded` count any record, including ones
///           filed at addresses no Safe lives at.
///         - The registry makes no external call.
///         - EVM CREATE2 only: chains that derive CREATE2 addresses
///           differently (zkSync Era) are out of scope.
contract SafeGenesisRegistry {
    uint8 public constant VERSION = 1;

    /// Upper bound on a stored initializer. A Safe `setup` with dozens of
    /// owners fits in a few KiB. The bound keeps a record writable in one
    /// transaction on every chain: a full 8 KiB record costs about 7M gas,
    /// well under the 16.78M per-transaction cap (EIP-7825).
    uint256 public constant MAX_INITIALIZER_LENGTH = 8_192;

    /// `createProxyWithNonce(address,bytes,uint256)`.
    bytes4 public constant CREATE_PROXY_WITH_NONCE = 0x1688f0b9;

    struct Genesis {
        /// The proxy factory that deploys the Safe.
        address factory;
        /// The Safe singleton (mastercopy) the proxy delegates to.
        address singleton;
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
    }

    mapping(address safe => Genesis) private _genesis;

    uint256 public totalRecords;

    event GenesisRecorded(
        address indexed safe,
        address indexed factory,
        address indexed singleton,
        uint256 saltNonce,
        bytes32 proxyCreationCodeHash,
        bytes initializer
    );

    error ZeroAddress();
    error EmptyProxyCreationCode();
    error EmptyInitializer();
    error InitializerTooLong(uint256 length);

    // -----------------------------------------------------------------------
    // Write
    // -----------------------------------------------------------------------

    /// File the genesis of the Safe these parameters create, under the
    /// address they create it at. Returns that address; a caller that
    /// expects a specific Safe compares it. Write-once and idempotent: the
    /// same parameters again return `created = false`, and no other
    /// parameters can reach the same address.
    ///
    /// `proxyCreationCode` is the factory's `proxyCreationCode()`. It is
    /// hashed, not stored: a wrong one only moves the record to an address
    /// no Safe lives at.
    function record(
        address factory,
        bytes calldata proxyCreationCode,
        address singleton,
        bytes calldata initializer,
        uint256 saltNonce
    ) external returns (address safe, bool created) {
        if (factory == address(0) || singleton == address(0)) revert ZeroAddress();
        if (proxyCreationCode.length == 0) revert EmptyProxyCreationCode();
        if (initializer.length == 0) revert EmptyInitializer();
        if (initializer.length > MAX_INITIALIZER_LENGTH) revert InitializerTooLong(initializer.length);

        safe = _address(factory, proxyCreationCode, singleton, initializer, saltNonce);
        if (_genesis[safe].factory != address(0)) return (safe, false);

        bytes32 proxyCreationCodeHash = keccak256(proxyCreationCode);
        _genesis[safe] = Genesis({
            factory: factory,
            singleton: singleton,
            saltNonce: saltNonce,
            initializer: initializer,
            proxyCreationCodeHash: proxyCreationCodeHash,
            recorder: msg.sender,
            recordedAt: uint64(block.number)
        });
        ++totalRecords;
        emit GenesisRecorded(safe, factory, singleton, saltNonce, proxyCreationCodeHash, initializer);
        return (safe, true);
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
