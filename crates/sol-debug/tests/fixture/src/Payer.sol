// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface ISink {
    function take(uint256 x) external payable returns (uint256);
}

/// Calls that carry ETH: a low-level `call{value:}` and an interface call with `{value:, gas:}`.
contract Payer {
    ISink public sink;

    function pay(address to) external payable returns (bool) {
        (bool ok,) = to.call{value: msg.value}("");
        require(ok, "send failed");
        return ok;
    }

    function forward(uint256 x) external payable returns (uint256) {
        return sink.take{value: msg.value, gas: 50000}(x);
    }
}
