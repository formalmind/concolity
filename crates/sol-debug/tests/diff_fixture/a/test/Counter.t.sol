// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Counter} from "../src/Counter.sol";

/// No forge-std: the debugger runs setUp() and the test itself.
contract CounterTest {
    Counter counter;

    function setUp() public {
        counter = new Counter();
    }

    function test_bumpTwice() public {
        counter.bump();
        require(counter.bump() == 2, "count");
    }
}
