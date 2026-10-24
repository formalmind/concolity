// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// The Fuzzing Book's `triangle` (Concolic Fuzzing chapter), in Solidity. Like the book's, it never checks that the
/// sides make a triangle, so concolity should find inputs that classify a non-triangle: the "bug" every path exposes.
contract Triangle {
    enum Kind { Scalene, Isosceles, Equilateral }

    function triangle(int256 a, int256 b, int256 c) external pure returns (Kind) {
        if (a == b) {
            if (b == c) return Kind.Equilateral;
            return Kind.Isosceles;
        }
        if (b == c) return Kind.Isosceles;
        if (a == c) return Kind.Isosceles;
        return Kind.Scalene;
    }

    /// The oracle: what a caller expects of any classified input.
    function check(int256 a, int256 b, int256 c) external view {
        this.triangle(a, b, c);
        require(a > 0 && b > 0 && c > 0, "not a side");
        require(a + b > c && b + c > a && a + c > b, "not a triangle");
    }
}
