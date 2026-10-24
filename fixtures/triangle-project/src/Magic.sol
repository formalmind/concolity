// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// A branch random fuzzing doesn't find: a magic value, then an exact equation. Behind it, an assert that fails
/// for most of the inputs that get there: the finding the explore loop should report.
contract Magic {
    uint256 public hits;

    function poke(uint256 x, uint256 y, uint256 z) external {
        if (x == 0x5eed1e55) {
            if (y * 3 == z + 7) {
                hits += 1;
                assert(z < 1_000_000);
            }
        }
    }
}
