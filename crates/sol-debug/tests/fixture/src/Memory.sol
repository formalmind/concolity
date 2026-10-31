// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Owned {
    address public owner;
}

library Batches {
    struct Item {
        address to;
        uint256 amount;
    }

    struct Batch {
        Item[4] items;
        uint256 count;
    }

    /// Writes through a memory parameter: the caller's batch changes.
    function add(Batch memory b, address to, uint256 amount) internal pure {
        for (uint256 i; i < b.count; ++i) {
            Item memory item = b.items[i];
            if (item.to == to) {
                b.items[i].amount += amount;
                return;
            }
        }
        b.items[b.count++] = Item(to, amount);
    }
}

library Text {
    function same(string storage store) internal pure returns (string storage) {
        return store;
    }

    function read(string storage store) internal view returns (string memory) {
        return store;
    }
}

/// Memory shared with callees, memory read in assembly, low-level static calls, getters and literals.
contract Memory {
    struct Ratio {
        uint256 n;
        uint256 d;
    }

    string internal name;
    uint256 public total;

    function batch(address a, address b) external returns (uint256) {
        Batches.Batch memory x;
        Batches.add(x, a, 1);
        Batches.add(x, b, 2);
        Batches.add(x, a, 3);
        total = x.items[0].amount;
        return x.count;
    }

    /// Another name for the same memory: the model can't follow it.
    function aliased(address a) external pure returns (uint256) {
        Batches.Batch memory x;
        Batches.Batch memory y = x;
        Batches.add(x, a, 1);
        return y.count;
    }

    function den(Ratio memory r) public pure returns (uint256 v) {
        assembly {
            v := mload(add(r, 32))
        }
    }

    function digest(bytes32 a, bytes32 b) public pure returns (bytes32 d) {
        assembly {
            let p := mload(0x40)
            mstore(p, hex"1901")
            mstore(add(p, 2), a)
            mstore(add(p, 34), b)
            d := keccak256(p, 66)
        }
    }

    function probe(address t, bytes memory data) external view returns (bool ok, bytes32 w) {
        (bool s, bytes memory r) = t.staticcall(data);
        ok = s && r.length >= 32;
        w = bytes32(bytes4(0x12345678));
    }

    function ownerOf(Owned o) external view returns (address) {
        return o.owner();
    }

    function tag() external pure returns (bytes1, uint256[] memory) {
        return (hex"0f", new uint256[](0));
    }

    function slot(bytes32 id) external pure returns (bytes32) {
        return keccak256(abi.encodePacked(id, uint256(6)));
    }

    function stored() external view returns (string memory) {
        return Text.read(name);
    }
}
