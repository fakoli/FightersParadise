//! # fp-vm
//!
//! Bytecode compiler and stack-based virtual machine for evaluating MUGEN
//! trigger expressions. Expressions in CNS files are compiled at load time
//! into compact bytecode and executed at runtime via a stack-based interpreter.

#![warn(missing_docs)]
