// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// Dynamic ABI arrays of static elements: encoded, decoded, and taken as calldata.
contract Arrays {
    struct Item {
        address who;
        uint96 amount;
        bool ok;
    }

    uint256 public total;

    function encode(Item[] memory items, uint256[] memory xs) public pure returns (bytes memory) {
        return abi.encode(items, xs);
    }

    function decode(bytes calldata data) external returns (uint256) {
        (Item[] memory items, uint256[] memory xs) = abi.decode(data, (Item[], uint256[]));
        total = items.length + xs.length;
        return total;
    }

    function sum(uint256[] calldata xs) external pure returns (uint256 s) {
        for (uint256 i; i < xs.length; ++i) {
            s += xs[i];
        }
    }

    /// Two loops with the same counter name: Lean won't shadow a mutable variable.
    function twice(uint256[] memory xs) public pure returns (uint256 s) {
        for (uint256 i; i < xs.length; ++i) {
            s += xs[i];
        }
        for (uint256 i; i < xs.length; ++i) {
            s += xs[i];
        }
    }
}
