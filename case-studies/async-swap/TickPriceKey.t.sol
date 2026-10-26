// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

import {Test} from "forge-std/Test.sol";
import {TickPrice} from "@async-swap/libraries/TickPrice.sol";
import {TickMath} from "v4-core/libraries/TickMath.sol";

/// @notice `TickPrice.key` is a function of the exact price only: an order's level can't depend on how large its
///         amounts are. Every other test of the key uses amounts whose product stays small, so a key that went wrong
///         for large orders (100 tokens a side at 18 decimals is already past 2^128) would pass them.
contract TickPriceKeyTest is Test {
    uint256 constant LAST_KEY = 2 * 887272 + 1;

    /// @dev Both amounts scaled by k: the same exact price, so the same key, on both sides of the book and for both
    ///      pool directions. floorTick picks its regime by `num / den`, which scaling leaves alone.
    function testFuzz_key_dependsOnlyOnThePrice(bool isBuy, bool zeroForOne, uint256 num, uint256 den, uint256 k)
        public
        pure
    {
        num = bound(num, 1, type(uint96).max);
        den = bound(den, 1, type(uint96).max);
        k = bound(k, 1, type(uint128).max);
        assertEq(
            TickPrice.key(isBuy, zeroForOne, num * k, den * k),
            TickPrice.key(isBuy, zeroForOne, num, den),
            "scaling both amounts moved the key"
        );
    }

    /// @dev Real order sizes: 100 tokens for 200 (18 decimals each) sit on the level of 1 for 2, not on an end key.
    function test_key_ofLargeOrders() public pure {
        for (uint256 i; i < 4; ++i) {
            (bool isBuy, bool zeroForOne) = (i & 1 == 1, i & 2 == 2);
            uint128 small = TickPrice.key(isBuy, zeroForOne, 1, 2);
            uint128 large = TickPrice.key(isBuy, zeroForOne, 100e18, 200e18);
            assertEq(large, small, "100e18 for 200e18 is the price of 1 for 2");
            assertTrue(large != 1 && large != LAST_KEY, "a price of 2 or 1/2 is far from the end keys");
            // A million tokens for a million: price 1, tick 0, the middle key.
            assertEq(TickPrice.key(isBuy, zeroForOne, 1e24, 1e24), uint128(uint24(-TickMath.MIN_TICK)) + 1, "price 1 is tick 0");
        }
    }
}
