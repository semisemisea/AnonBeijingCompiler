use raana_ir::ir::InstKind;
use taki_mir::{
    labels::TargetSymbol,
    prelude::{Arena, HirProgram},
};

const INLINE_MEMZERO_MAX_STORES: usize = 4;

/// Compiler-provided symbols emitted into the same assembly unit as user code.
///
/// These are intentionally distinct from source-level functions and external
/// libc/runtime symbols. Every variant must have a private, collision-safe
/// local assembly label and a matching entry in `EMBEDDED_ASSEMBLIES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedSymbol {
    Memset,
}

impl TargetSymbol for EmbeddedSymbol {
    fn symbol(self) -> &'static str {
        match self {
            Self::Memset => ".Lsoyo_memzero",
        }
    }
}

struct EmbeddedAssembly {
    symbol: EmbeddedSymbol,
    source: &'static str,
    is_required: fn(&HirProgram) -> bool,
}

const EMBEDDED_ASSEMBLIES: &[EmbeddedAssembly] = &[EmbeddedAssembly {
    symbol: EmbeddedSymbol::Memset,
    source: include_str!("runtime/memzero.S"),
    is_required: needs_memset,
}];

pub fn mem_zero_is_inline(byte_len: usize) -> bool {
    byte_len % 4 == 0 && byte_len / 4 <= INLINE_MEMZERO_MAX_STORES
}

pub fn assembly(program: &HirProgram) -> Option<String> {
    let mut output = String::new();
    for fragment in EMBEDDED_ASSEMBLIES {
        if (fragment.is_required)(program) {
            debug_assert!(
                fragment.source.contains(fragment.symbol.symbol()),
                "embedded assembly for {:?} does not define {}",
                fragment.symbol,
                fragment.symbol.symbol()
            );
            output.push_str(fragment.source);
            if !fragment.source.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    (!output.is_empty()).then_some(output)
}

fn needs_memset(program: &HirProgram) -> bool {
    program.function_layout().iter().any(|&function| {
        program
            .func_data(function)
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|block| block.insts().iter())
            .any(
                |&inst| match program.func_data(function).inst_data(inst).kind() {
                    InstKind::MemZero(mem_zero) => !mem_zero_is_inline(mem_zero.byte_len()),
                    _ => false,
                },
            )
    })
}
