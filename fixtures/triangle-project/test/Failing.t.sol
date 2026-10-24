// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Triangle} from "../src/Triangle.sol";

/// Fails on purpose, for the debugger: `check` rejects a side of 0.
contract TriangleFailingTest {
    Triangle t;

    function setUp() public {
        t = new Triangle();
    }

    function test_zeroSide() public view {
        t.check(0, 2, 2);
    }
}
