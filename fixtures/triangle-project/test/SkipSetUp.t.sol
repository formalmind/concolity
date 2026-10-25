// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface Vm {
    function skip(bool) external;
}

/// A setUp that skips: forge reports one skipped `setUp()`.
contract SkipSetUpTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    function setUp() public {
        vm.skip(true);
    }

    function test_never_runs() public pure {}
}
