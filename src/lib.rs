//! concolity: a concolic tracer for EVM bytecode. revm runs every opcode for real; the [`shadow::Shadow`]
//! inspector keeps a symbolic term ([`expr`]) next to each value that depends on the inputs, following the model in
//! EVM-MODEL.md; [`smt`] hands path conditions to z3. See ARCHITECTURE.md.

pub mod chain;
pub mod coverage;
pub mod explore;
pub mod expr;
pub mod reduce;
pub mod sarif;
pub mod shadow;
pub mod smt;
pub mod versions;
