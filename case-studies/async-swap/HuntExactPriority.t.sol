// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

import {PriorityMatchingInvariantTest, MatchingHandler} from "./MatchingInvariant.t.sol";
import {StdInvariant} from "forge-std/StdInvariant.sol";
import {console} from "forge-std/console.sol";

/// @dev Orders packed around one price ($2,000, within about one tick), so several share a tick level with different
///      exact limits and bids: the case the exact-priority fix is about, which the main handler's $1,000-$4,000 spread
///      almost never produces (about 14,000 levels for a few dozen orders).
contract SameLevelHandler {
    MatchingHandler immutable main;

    constructor(MatchingHandler h) {
        main = h;
    }

    function submitNear(uint256 who, bool sell, uint256 size, uint256 jitter, uint256 fee) external {
        main.submit(who, sell, size, 2_000e6 + jitter % 200_000, fee);
    }
}
import {PoolIdLibrary} from "v4-core/types/PoolKey.sol";

/// @notice The AuditAgent-2 fix, as an invariant: the book's best order on each side is the best exact price, then the
///         higher bid, then the earlier sequence (the priority key), found by a linear scan. The existing
///         `invariant_bestPricesAgreeWithLinearScan` compares only tick levels, which can't see a wrong pick inside a
///         level. Priority mode, so bids vary and ties on price and bid happen.
contract HuntExactPriorityInvariantTest is PriorityMatchingInvariantTest {
    using PoolIdLibrary for *;

    struct Ord {
        bytes32 id;
        uint256 amountIn;
        uint256 amountOutMin;
        uint64 sequence;
        uint64 fee;
    }

    SameLevelHandler near;

    function setUp() public override {
        super.setUp();
        near = new SameLevelHandler(handler);
        bytes4[] memory selectors = new bytes4[](1);
        selectors[0] = SameLevelHandler.submitNear.selector;
        targetSelector(StdInvariant.FuzzSelector(address(near), selectors));
        targetContract(address(near));
    }

    /// @dev How many open orders share a tick level with another on the same side (the campaign must make them).
    function _shared() internal view returns (uint256 n) {
        (bytes32[] memory ids,) = handler.book();
        for (uint256 i; i < ids.length; ++i) {
            (, uint256 ai,, uint128 pi, bool bi,,) = hook.orderBookOrder(key.toId(), ids[i]);
            if (ai == 0) continue;
            for (uint256 j = i + 1; j < ids.length; ++j) {
                (, uint256 aj,, uint128 pj, bool bj,,) = hook.orderBookOrder(key.toId(), ids[j]);
                if (aj != 0 && pi == pj && bi == bj) {
                    ++n;
                    break;
                }
            }
        }
    }

    /// @dev The largest number of open orders sharing a level with another, seen at any invariant check.
    uint256 public maxShared;

    /// @dev At the end of each run: how much same-level book the campaign made (-vv shows it).
    function afterInvariant() public {
        uint256 n = _shared();
        if (n > maxShared) maxShared = n;
        console.log("orders sharing a tick level at the end of this run:", n);
    }

    /// @dev Strictly ahead of `b` in the book: a better exact limit, then a higher bid, then an earlier sequence.
    function _ahead(bool buy, Ord memory a, Ord memory b) internal pure returns (bool) {
        // Quote per asset: a bid offers amountIn quote for amountOutMin asset (higher is better); an ask wants
        // amountOutMin quote for amountIn asset (lower is better). Cross-multiplied, so no rounding.
        uint256 l = buy ? a.amountIn * b.amountOutMin : b.amountOutMin * a.amountIn;
        uint256 r = buy ? b.amountIn * a.amountOutMin : a.amountOutMin * b.amountIn;
        if (l != r) return l > r;
        if (a.fee != b.fee) return a.fee > b.fee;
        return a.sequence < b.sequence;
    }

    function _scan(bool buy) internal view returns (bool found, Ord memory best) {
        (bytes32[] memory ids,) = handler.book();
        for (uint256 i; i < ids.length; ++i) {
            (, uint256 amountIn, uint256 amountOutMin,, bool isBuy, uint64 sequence, uint64 fee) =
                hook.orderBookOrder(key.toId(), ids[i]);
            if (amountIn == 0 || isBuy != buy) continue;
            Ord memory o = Ord(ids[i], amountIn, amountOutMin, sequence, fee);
            if (!found || _ahead(buy, o, best)) {
                best = o;
                found = true;
            }
        }
    }

    function invariant_bestOrderIsExactThenBidThenAge() public view {
        for (uint256 side; side < 2; ++side) {
            bool buy = side == 0;
            (bool found, Ord memory expected) = _scan(buy);
            (bool hookFound,, bytes32 hookId,) = hook.bestOrder(key.toId(), buy);
            assertEq(hookFound, found, buy ? "a bid exists" : "an ask exists");
            if (found) assertEq(hookId, expected.id, buy ? "best bid: exact price, then bid, then age" : "best ask: exact price, then bid, then age");
        }
    }
}
