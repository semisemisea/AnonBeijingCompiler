use raana_ir::ir::InstKind;
use raana_ir::ir::inst_kind::MemZeroLen;
use taki_mir::prelude::{Arena, HirProgram};

const INLINE_MEMZERO_MAX_STORES: usize = 4;

/// Compiler-provided symbols emitted into the same assembly unit as user code.
///
/// These are intentionally distinct from source-level functions and external
/// libc/runtime symbols. Every variant must have a private, collision-safe
/// local assembly label and a matching entry in `EMBEDDED_ASSEMBLIES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedSymbol {
    Memset,
    /// Zero-extends its two 32-bit arguments and tail-calls glibc `calloc`
    /// (used by the M68 recursive-memoization pass).
    Calloc,
}

impl EmbeddedSymbol {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Memset => ".Lsoyo_memzero",
            Self::Calloc => ".Lsoyo_calloc",
        }
    }
}

struct EmbeddedAssembly {
    symbol: EmbeddedSymbol,
    source: &'static str,
    is_required: fn(&HirProgram) -> bool,
}

const EMBEDDED_ASSEMBLIES: &[EmbeddedAssembly] = &[
    EmbeddedAssembly {
        symbol: EmbeddedSymbol::Memset,
        source: include_str!("runtime/memzero.S"),
        is_required: needs_memset,
    },
    EmbeddedAssembly {
        symbol: EmbeddedSymbol::Calloc,
        source: include_str!("runtime/calloc.S"),
        is_required: needs_calloc,
    },
];

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
                    InstKind::MemZero(mem_zero) => match mem_zero.byte_len_len() {
                        MemZeroLen::Const(byte_len) => !mem_zero_is_inline(*byte_len),
                        MemZeroLen::Value(_) => true,
                    },
                    _ => false,
                },
            )
    })
}

/// True when the program calls the compiler-provided `soyo_calloc` allocator
/// declared by the M68 recursive-memoization pass.
fn needs_calloc(program: &HirProgram) -> bool {
    program.function_layout().iter().any(|&function| {
        program
            .func_data(function)
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|block| block.insts().iter())
            .any(|&inst| {
                matches!(
                    program.func_data(function).inst_data(inst).kind(),
                    InstKind::Call(call)
                        if program.func_data(call.callee()).name()
                            == raana_ir::opt::CALLOO_NAME
                )
            })
    })
}
