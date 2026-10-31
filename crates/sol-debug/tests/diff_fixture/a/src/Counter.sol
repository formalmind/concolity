// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Counter {
    event Bumped(address by, uint256 count);

    uint256 public count;
    mapping(address => uint256) public bumps;

    function bump() external returns (uint256) {
        count += 1;
        bumps[msg.sender] += 1;
        emit Bumped(msg.sender, count);
        return count;
    }
}
