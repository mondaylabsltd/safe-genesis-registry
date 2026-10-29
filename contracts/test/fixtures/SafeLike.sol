// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// Test fixtures that follow Safe's proxy factory logic (v1.3.0 / v1.4.1:
/// `salt = keccak256(keccak256(initializer) ‖ saltNonce)`, init code =
/// proxy creation code ‖ uint256(singleton)). They are NOT the canonical
/// bytecode — the fork tests use the real deployments — but they exercise the
/// same CREATE2 shape without a network.

interface IProxyCreationCallback {
    function proxyCreated(address proxy, address singleton, bytes calldata initializer, uint256 saltNonce) external;
}

contract SafeLikeProxy {
    address internal singleton;

    constructor(address _singleton) {
        require(_singleton != address(0), "Invalid singleton address provided");
        singleton = _singleton;
    }

    fallback() external payable {
        address target = singleton;
        assembly {
            calldatacopy(0, 0, calldatasize())
            let ok := delegatecall(gas(), target, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            if iszero(ok) { revert(0, returndatasize()) }
            return(0, returndatasize())
        }
    }
}

contract SafeLikeProxyFactory {
    event ProxyCreation(address indexed proxy, address singleton);

    function proxyCreationCode() public pure returns (bytes memory) {
        return type(SafeLikeProxy).creationCode;
    }

    function createProxyWithNonce(address singleton, bytes memory initializer, uint256 saltNonce)
        public
        returns (address proxy)
    {
        bytes32 salt = keccak256(abi.encodePacked(keccak256(initializer), saltNonce));
        proxy = _deploy(singleton, initializer, salt);
    }

    /// v1.3.0 / v1.4.1: the callback is folded into the salt nonce.
    function createProxyWithCallback(
        address singleton,
        bytes memory initializer,
        uint256 saltNonce,
        IProxyCreationCallback callback
    ) public returns (address proxy) {
        uint256 saltNonceWithCallback = uint256(keccak256(abi.encodePacked(saltNonce, callback)));
        proxy = createProxyWithNonce(singleton, initializer, saltNonceWithCallback);
        if (address(callback) != address(0)) callback.proxyCreated(proxy, singleton, initializer, saltNonce);
    }

    /// v1.4.1's chain-specific variant: the chain id is part of the salt, so
    /// the address differs on every chain.
    function createChainSpecificProxyWithNonce(address singleton, bytes memory initializer, uint256 saltNonce)
        public
        returns (address proxy)
    {
        bytes32 salt = keccak256(abi.encodePacked(keccak256(initializer), saltNonce, block.chainid));
        proxy = _deploy(singleton, initializer, salt);
    }

    function _deploy(address singleton, bytes memory initializer, bytes32 salt) private returns (address proxy) {
        bytes memory deploymentData = abi.encodePacked(type(SafeLikeProxy).creationCode, uint256(uint160(singleton)));
        assembly {
            proxy := create2(0x0, add(0x20, deploymentData), mload(deploymentData), salt)
        }
        require(proxy != address(0), "Create2 call failed");
        if (initializer.length > 0) {
            assembly {
                if eq(call(gas(), proxy, 0, add(initializer, 0x20), mload(initializer), 0, 0), 0) { revert(0, 0) }
            }
        }
        emit ProxyCreation(proxy, singleton);
    }
}

/// A singleton with Safe's `setup` signature and an owner list that can
/// change after creation — the case the registry exists for.
contract MockSafeSingleton {
    /// Slot 0 belongs to the proxy's singleton pointer, as in Safe's own
    /// `Singleton` base contract.
    address private singletonSlot;
    address[] internal owners;
    uint256 internal threshold;

    function setup(
        address[] calldata _owners,
        uint256 _threshold,
        address,
        bytes calldata,
        address,
        address,
        uint256,
        address payable
    ) external {
        require(threshold == 0, "GS200");
        require(_threshold > 0 && _threshold <= _owners.length, "GS201");
        owners = _owners;
        threshold = _threshold;
    }

    function swapOwner(uint256 position, address newOwner) external {
        owners[position] = newOwner;
    }

    /// The Safe making a call of its own, as `execTransaction` does once its
    /// owners approve (here: no signatures, the mock trusts any caller).
    function call(address to, bytes calldata data) external {
        (bool ok, bytes memory ret) = to.call(data);
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
    }

    function getOwners() external view returns (address[] memory) {
        return owners;
    }

    function getThreshold() external view returns (uint256) {
        return threshold;
    }
}

contract CountingCallback is IProxyCreationCallback {
    uint256 public calls;

    function proxyCreated(address, address, bytes calldata, uint256) external {
        ++calls;
    }
}
