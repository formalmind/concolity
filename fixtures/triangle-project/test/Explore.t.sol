// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Triangle} from "../src/Triangle.sol";

/// Fuzz harnesses for the explore loop: their arguments are what concolity solves for.
contract TriangleExploreTest {
    Triangle t;

    function setUp() public {
        t = new Triangle();
    }

    function testFuzz_triangle(int256 a, int256 b, int256 c) public view {
        t.triangle(a, b, c);
    }

    /// check() reverts on non-triangles; a Panic (overflow) is the kind of thing explore should find.
    function testFuzz_check(int256 a, int256 b, int256 c) public view {
        try t.check(a, b, c) {} catch Error(string memory) {}
    }
}

import {Magic} from "../src/Magic.sol";

contract MagicExploreTest {
    Magic m;

    function setUp() public {
        m = new Magic();
    }

    function testFuzz_poke(uint256 x, uint256 y, uint256 z) public {
        m.poke(x, y, z);
    }
}
