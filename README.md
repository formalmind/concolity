# concolity

A concolic tracer for the EVM. It runs your contracts for real in [revm](https://github.com/bluealloy/revm), with
real state and real cheatcodes. Next to every value it keeps a symbolic expression over the inputs, and it records
every branch condition. Then it flips a branch nobody has taken, asks an SMT solver (z3 or bitwuzla) for inputs that
take it, and runs again.

Fuzzers guess, and they rarely hit `x == 0x5eed1e55`, a narrow arithmetic window or a chain of `require`s.
Symbolic tools reason about every path from the start, and they stall in deep protocols. concolity sits between the
two: it runs as fast as a fuzzer and solves one branch at a time.

Branches are reported in Solidity, by line and AST node, through
[sol-debug](https://crates.io/crates/sol-debug)'s source maps. Every solved input is a call sequence that replays in
the debugger and becomes a Foundry test. It also runs on Huff and on bare bytecode, where it reports by pc.

The design follows the `ConcolicTracer` of [The Fuzzing Book](https://www.fuzzingbook.org/html/ConcolicFuzzer.html),
moved from Python values into the EVM interpreter. Read [DESIGN.md](DESIGN.md).

**Status:** design. The first milestone is one transaction with symbolic calldata, z3 as the solver, and the book's
own examples ported to Solidity as fixtures.

Part of [Formal Mind](https://formal.team).

## License

MIT
