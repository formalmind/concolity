// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {MathLib} from "dep/MathLib.sol";
import {Unused} from "./Unused.sol";

interface IOracle {
    function price() external view returns (uint256);
}

interface IToken {
    function transfer(address to, uint256 amount) external returns (bool);
}

interface IVault {
    function total() external view returns (uint256);
}

abstract contract Base {
    uint256 public baseCount;

    function _hook() internal virtual {
        baseCount += 1;
    }

    function bump() public virtual {
        _hook();
    }
}

contract Vault is Base, IVault {
    using MathLib for uint256;

    struct Account {
        uint256 shares;
        uint256 debt;
    }

    uint256 public constant FEE = 3;
    uint256 public constant MAX_FEE = 0.25e18;
    uint256 public constant CAP = 1.5 ether;
    uint256 public override total;
    mapping(address => Account) internal accounts;
    uint256[] internal history;
    uint256 internal locked;
    IOracle public oracle;
    IToken public token;
    address public owner;

    modifier onlyOwner() {
        require(msg.sender == owner, "owner");
        _;
    }

    function deposit(uint256 assets) external {
        if (assets == 0) revert();
        Account storage acc = accounts[msg.sender];
        uint256 shares = assets.mulDiv(1e18, oracle.price());
        _credit(acc, shares);
        total += assets;
        history.push(assets);
    }

    function clearDebt(bool mine) external {
        Account storage acc = mine ? accounts[msg.sender] : accounts[owner];
        acc.debt = 0;
    }

    function _credit(Account storage acc, uint256 shares) internal {
        acc.shares += shares;
    }

    function withdraw(uint256 shares) external {
        uint256 assets = shares.mulDivUp(oracle.price(), 1e18);
        token.transfer(msg.sender, assets);
        accounts[msg.sender].shares -= shares;
        unchecked {
            total = total - assets;
        }
    }

    function lockSlot() external onlyOwner {
        assembly {
            sstore(locked.slot, 1)
        }
    }

    function _hook() internal override {
        super._hook();
        locked = 0;
    }

    function setOwner(address next) external onlyOwner {
        owner = next;
    }

    function limits() external pure returns (uint256, uint256) {
        return (MAX_FEE, CAP);
    }
}
