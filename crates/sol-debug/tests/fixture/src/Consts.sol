// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// EIP-712 style type strings: constants built from literals and other constants.
library Sigs {
    bytes internal constant A_TYPE = "A(uint256 x)";
    bytes32 internal constant A_HASH = keccak256(A_TYPE);
    bytes32 internal constant B_HASH = keccak256(abi.encodePacked("B(A a)", A_TYPE));
    string internal constant WITNESS = string(abi.encodePacked("B witness)", A_TYPE));
}

contract Consts {
    function hashes() external pure returns (bytes32, bytes32) {
        return (Sigs.A_HASH, Sigs.B_HASH);
    }

    function witness() external pure returns (string memory) {
        return Sigs.WITNESS;
    }
}
