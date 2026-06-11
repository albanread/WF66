//! LET - a small infix-algebraic DSL embedded in Forth.
//!
//! `LET ( in1, in2, ... ) -> ( out1, out2, ... ) = expr_list WHERE ... END`
//!
//! Compiles to a stand-alone Win64 function `(inputs*, outputs*) -> ()`.
//! The Forth `LET` immediate word reads source up to `END`, calls [`compile`],
//! assembles the resulting text through JASM/Rasm, places the function in the
//! session code arena, and emits a small trampoline at HERE that loads the
//! inputs from the Forth FP stack, calls the compiled function, and pushes the
//! outputs back.
//!
//! Scope of the MVP:
//! * Operators: `+ - * /` and unary `-`.
//! * Constants: `pi`, `e`, plus any numeric literals.
//! * WHERE bindings with dependency-ordered eval and cycle detection.
//! * Multiple inputs and multiple outputs.
//!
//! Not yet implemented (future work):
//! * Function calls (`sin`, `cos`, `sqrt`, `pow`, `hypot`, ...).
//! * `**` operator (needs `pow`).
//! * `select(cond, a, b)` / `IF/THEN/ELSE`.

pub mod parser;
pub mod codegen;

use std::collections::HashMap;

pub use parser::{LetError, LetForm};

/// Map from libm function name (e.g. "sin") to its absolute address in
/// the host process. Populated by the caller (typically the WF64 runtime)
/// before calling [`compile`], so the LET codegen can bake direct
/// `mov rax, addr ; call rax` sequences without relying on dynamic symbol
/// resolution in the assembler.
pub type LibmTable = HashMap<String, u64>;

/// Result of compiling one LET form to MC-flavour Intel asm text.
#[derive(Debug)]
pub struct CompiledLet {
    /// Function name as emitted into the asm. Must be unique in the session.
    pub fn_name: String,
    /// Asm source text ready for JASM/Rasm assembly.
    pub asm_text: String,
    /// Number of inputs the function reads from `[rcx + i*8]`.
    pub n_inputs: usize,
    /// Number of outputs the function writes to `[rdx + i*8]` (with
    /// `outputs[0]` being the rightmost result = FP stack TOS).
    pub n_outputs: usize,
}

/// Parse and lower a LET form. Does not assemble or place executable code.
///
/// `libm_table`, if present, supplies absolute addresses for libm
/// functions the LET source may reference (sin, cos, sqrt, pow, ...).
/// Missing entries cause an error at lower time. Pass an empty map
/// for LETs that don't use any libm functions.
pub fn compile(source: &str, fn_name: &str, libm_table: &LibmTable) -> Result<CompiledLet, LetError> {
    let form = parser::parse(source)?;
    let asm_text = codegen::lower(&form, fn_name, libm_table)?;
    Ok(CompiledLet {
        fn_name: fn_name.to_string(),
        asm_text,
        n_inputs: form.inputs.len(),
        n_outputs: form.outputs.len(),
    })
}
