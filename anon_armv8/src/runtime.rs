//! 编译器内嵌运行时符号与汇编片段。
//!
//! 本模块持有编译器自己注入的符号（[`EmbeddedSymbol`]）及对应汇编片段
//! （`runtime/` 目录下的 `.S` 文件，经 `include_str!` 编译期嵌入，与用户代码
//! 输出到同一个汇编单元）。这些符号**刻意区别于**用户源码函数与外部 libc 符号：
//!
//! - [`EmbeddedSymbol::Memset`]：`MemZero`/`MemZeroLen` 指令展开时的内存清零助手，
//!   小尺寸内联为 store 序列（`INLINE_MEMZERO_MAX_STORES`），大尺寸调用此符号；
//! - [`EmbeddedSymbol::Calloc`]：零扩展两个 32 位参数后尾调用 glibc `calloc`
//!   （递归记忆化 IR pass（里程碑 M68）为缓存分配内存时使用）。
//!
//! 每个变体必须有私有、碰撞安全的本地汇编标签，并在 `EMBEDDED_ASSEMBLIES`
//! 里有对应条目。新增内嵌符号时三个位置要同步：枚举变体、标签映射、
//! 汇编片段。

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
