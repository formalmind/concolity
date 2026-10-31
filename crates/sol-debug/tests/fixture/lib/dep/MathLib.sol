// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

library MathLib {
    function mulDivUp(uint256 a, uint256 b, uint256 c) internal pure returns (uint256) {
        return (a * b + c - 1) / c;
    }

    function mulDiv(uint256 a, uint256 b, uint256 c) internal pure returns (uint256) {
        return a * b / c;
    }
}
