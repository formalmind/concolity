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
moved from Python values into the EVM interpreter. It works on bytecode; source maps and the AST only add names. Read [ARCHITECTURE.md](ARCHITECTURE.md), [DESIGN.md](DESIGN.md), and the first case study: [Async Swap's unreachable guards](case-studies/async-swap/).

**Status:** the EVM model runs ([EVM-MODEL.md](EVM-MODEL.md#status-2026-10-24)): revm executes, every opcode's
result gets its symbolic term, every step is checked against revm, and z3 solves flipped branches. On the book's
triangle every branch flips to an input that takes the other side. Next: the trace tree, so it explores on its own.

```
cargo test --release          # needs z3 on PATH
```

Part of [Formal Mind](https://formal.team).

## License

MIT
