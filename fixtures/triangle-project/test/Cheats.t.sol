// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Triangle} from "../src/Triangle.sol";

interface Vm {
    function store(address, bytes32, bytes32) external;
    function load(address, bytes32) external view returns (bytes32);
    function record() external;
    function accesses(address) external returns (bytes32[] memory reads, bytes32[] memory writes);
    function mockCall(address, bytes calldata, bytes calldata) external;
    function mockCallRevert(address, bytes calldata, bytes calldata) external;
    function clearMockedCalls() external;
    function chainId(uint256) external;
    function snapshotState() external returns (uint256);
    function revertToState(uint256) external returns (bool);
    function getNonce(address) external view returns (uint64);
    function computeCreateAddress(address, uint256) external pure returns (address);
    function getDeployedCode(string calldata) external view returns (bytes memory);
}

interface IOracle {
    function price() external view returns (uint256);
}

contract Counter {
    uint256 public count;

    function bump() external {
        count += 1;
    }
}

/// The cheatcodes sol-debug answers beyond the common ones, each checked the way forge behaves.
contract CheatsTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    Counter c;

    function setUp() public {
        c = new Counter();
    }

    function test_storeAndLoad() public {
        vm.store(address(c), bytes32(0), bytes32(uint256(41)));
        require(c.count() == 41, "store");
        require(vm.load(address(c), bytes32(0)) == bytes32(uint256(41)), "load");
    }

    function test_recordAccesses() public {
        vm.record();
        c.bump();
        (bytes32[] memory reads, bytes32[] memory writes) = vm.accesses(address(c));
        require(writes.length == 1 && writes[0] == bytes32(0), "writes");
        require(reads.length >= 2, "an SSTORE counts as a read too");
    }

    function test_mockCall() public {
        address oracle = address(0x0a11);
        vm.mockCall(oracle, abi.encodeWithSelector(IOracle.price.selector), abi.encode(uint256(7)));
        (bool ok, bytes memory out) = oracle.staticcall(abi.encodeWithSelector(IOracle.price.selector));
        require(ok && abi.decode(out, (uint256)) == 7, "mocked answer");
        vm.mockCallRevert(oracle, abi.encodeWithSelector(IOracle.price.selector), bytes("stale"));
        (ok,) = oracle.staticcall(abi.encodeWithSelector(IOracle.price.selector));
        require(!ok, "mocked revert");
        vm.clearMockedCalls();
        (ok, out) = oracle.staticcall(abi.encodeWithSelector(IOracle.price.selector));
        require(ok && out.length == 0, "cleared");
    }

    function test_chainId() public {
        vm.chainId(130);
        require(block.chainid == 130, "chain id");
    }

    function test_snapshot() public {
        uint256 id = vm.snapshotState();
        c.bump();
        require(c.count() == 1, "bumped");
        require(vm.revertToState(id), "reverted");
        require(c.count() == 0, "back to the snapshot");
    }

    function test_nonceAndAddresses() public {
        uint64 n = vm.getNonce(address(this));
        Counter d = new Counter();
        require(address(d) == vm.computeCreateAddress(address(this), n), "create address");
        require(keccak256(vm.getDeployedCode("Cheats.t.sol:Counter")) == keccak256(address(d).code), "deployed code");
    }
}
