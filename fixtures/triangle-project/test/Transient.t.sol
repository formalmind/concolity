// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

contract T {
    function set() external {
        assembly { tstore(0, 7) }
    }

    function get() external view returns (uint256 v) {
        assembly { v := tload(0) }
    }

    function setThenGet() external returns (uint256 v) {
        this.set();
        v = this.get();
    }
}

/// forge clears transient storage between the test's own top-level calls (each is its own transaction there),
/// but not inside one call.
contract TransientTest {
    T t;

    function setUp() public {
        t = new T();
    }

    function test_clearedBetweenTopLevelCalls() public {
        t.set();
        require(t.get() == 0, "cleared after the call returned");
    }

    function test_keptInsideOneCall() public {
        require(t.setThenGet() == 7, "kept within a call");
    }
}
