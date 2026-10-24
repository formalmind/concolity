// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Triangle} from "../src/Triangle.sol";

interface Vm {
    function assume(bool) external pure;
}

/// A suite that leaves some of Triangle uncovered on purpose: no scalene test, `check` only on a valid triangle.
contract TriangleTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    Triangle t;

    function setUp() public {
        t = new Triangle();
    }

    function test_equilateral() public view {
        require(t.triangle(1, 1, 1) == Triangle.Kind.Equilateral, "equilateral");
    }

    function test_isosceles() public view {
        require(t.triangle(2, 2, 3) == Triangle.Kind.Isosceles, "isosceles");
    }

    function test_check() public view {
        t.check(3, 3, 3);
    }

    function testFuzz_sameSides(int256 a) public view {
        vm.assume(a > 0);
        require(t.triangle(a, a, a) == Triangle.Kind.Equilateral, "same sides");
    }

    /// A call to an address with no code: the binding report must flag it.
    function test_missingOracle() public {
        (bool ok,) = address(0xdead).call(abi.encodeWithSignature("price()"));
        require(ok, "a call to an empty account succeeds");
    }
}

