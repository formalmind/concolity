// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

abstract contract Named {
    bytes32 internal immutable nameHash;
    uint256 internal count = 7;

    constructor(string memory name) {
        nameHash = keccak256(bytes(name));
    }
}

contract Child {
    address public immutable parent;

    constructor() {
        parent = msg.sender;
    }
}

/// A deployment: a base constructor with arguments, a state initializer, immutables and a contract creation.
contract Deploy is Named("deploy") {
    address public immutable owner;
    address public immutable child;
    uint256 public total;

    constructor(address _owner) {
        require(_owner != address(0), "zero");
        owner = _owner;
        child = address(new Child());
        total = count + 1;
    }

    function hash() external view returns (bytes32) {
        return nameHash;
    }
}
