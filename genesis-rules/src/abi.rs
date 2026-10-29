//! The contract interfaces the service speaks. Selectors and layouts come
//! from these declarations alone, never from hand-written constants.

alloy::sol! {
    /// The on-chain registry (contracts/src/SafeGenesisRegistry.sol).
    interface ISafeGenesisRegistry {
        struct Genesis {
            address factory;
            address singleton;
            uint256 saltNonce;
            bytes initializer;
            bytes32 proxyCreationCodeHash;
            address recorder;
            uint64 recordedAt;
            uint32 index;
            string name;
        }

        event GenesisRecorded(
            address indexed safe,
            address indexed factory,
            address indexed singleton,
            uint256 saltNonce,
            bytes32 proxyCreationCodeHash,
            bytes initializer,
            string name
        );

        function record(
            address factory,
            bytes proxyCreationCode,
            address singleton,
            bytes initializer,
            uint256 saltNonce,
            string name
        ) external returns (address safe, bool created);
        function genesisOf(address safe) external view returns (bool found, Genesis genesis);
        function isRecorded(address safe) external view returns (bool);
        function deployCallOf(address safe) external view returns (address factory, bytes data);
        function totalRecords() external view returns (uint256);
        function safeAt(uint256 index) external view returns (address);
        function genesisPage(uint256 offset, uint256 limit, bool desc)
            external view returns (uint256 total, address[] safes, Genesis[] records);
    }

    /// Safe's proxy factory, every version's creation entry points.
    interface ISafeProxyFactory {
        function proxyCreationCode() external pure returns (bytes);
        function createProxyWithNonce(address singleton, bytes initializer, uint256 saltNonce) external returns (address proxy);
        function createProxyWithNonceL2(address singleton, bytes initializer, uint256 saltNonce) external returns (address proxy);
        function createProxyWithCallback(address singleton, bytes initializer, uint256 saltNonce, address callback) external returns (address proxy);
        function createChainSpecificProxyWithNonce(address singleton, bytes initializer, uint256 saltNonce) external returns (address proxy);
        function createChainSpecificProxyWithNonceL2(address singleton, bytes initializer, uint256 saltNonce) external returns (address proxy);
        /// v1.0.0 – v1.1.1: plain CREATE, the address follows the factory's nonce.
        function createProxy(address singleton, bytes data) external returns (address proxy);
    }

    interface ISafe {
        function setup(
            address[] owners,
            uint256 threshold,
            address to,
            bytes data,
            address fallbackHandler,
            address paymentToken,
            uint256 payment,
            address paymentReceiver
        ) external;
        function getOwners() external view returns (address[]);
        function getThreshold() external view returns (uint256);
        function nonce() external view returns (uint256);
    }

    /// Setup hooks real Safes are created with.
    interface IMultiSend {
        function multiSend(bytes transactions) external payable;
    }
    interface ISafeToL2Setup {
        function setupToL2(address l2Singleton) external;
    }
    interface ISafeModuleSetup {
        function enableModules(address[] modules) external;
    }

    /// Safe's passkey module (v0.2.1), as Vela wallets use it during setup.
    interface ISafeWebAuthnSignerFactory {
        function createSigner(uint256 x, uint256 y, uint176 verifiers) external returns (address signer);
    }
    interface ISafeWebAuthnSharedSigner {
        struct Signer { uint256 x; uint256 y; uint176 verifiers; }
        function configure(Signer signer) external;
    }

    /// Reading a deployed Safe's authority beyond its owners.
    interface ISafeModules {
        function getModulesPaginated(address start, uint256 pageSize) external view returns (address[] array, address next);
    }
}
