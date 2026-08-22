//! # taki_mir：机器级中间表示（VCode）与代码生成管线
//!
//! 定位链：SysY 源码 → RaanaIR（平台无关 SSA，`raana_ir` crate）→ **VCode**
//! （机器指令级，本 crate）→ 汇编文本。taki_mir 是**后端共享**的机器级 MIR：
//! 夹在 `raana_ir`（平台无关 SSA IR）与目标汇编之间，AArch64（`anon_armv8`）
//! 与 RISC-V（`uika_riscv`）两个后端复用同一套 VCode 容器、寄存器分配器与
//! 发射基础设施，只需各自提供机器指令类型与 lower/emit 细节。
//!
//! ## 数据流
//!
//! ```text
//! RaanaIR（HirProgram，平台无关 SSA）
//!   → lower：指令选择，逐条把 RaanaIR 指令降成机器指令（操作数为虚拟寄存器）
//!   → VCode：机器指令级 IR（VCodeBuilder 构建，VCodeContainer 持有）
//!   → reg_alloc：虚拟寄存器 → 物理寄存器 / 栈槽（ion::run，ION 回溯分配器）
//!   → 回写 + finalize_for_emission（物化分配器 move、展开伪寻址）
//!   → emit：AsmWriter 组织，逐条 MachInstEmit::emit 打印汇编文本
//! ```
//!
//! MIR pass 挂在分配前后两个时点：**pre-RA**（lower 之后、reg_alloc 之前，
//! 操作对象仍是虚拟寄存器）与 **post-RA**（回写之后、发射之前，操作对象是
//! 物理寄存器与栈槽）。
//!
//! 顶层入口就在本文件：`compile` / `compile_with_config` 泛型于
//! `LowerBackend`，一次编译一个 `HirProgram`，产出 `CompileOutput`（汇编文本
//! + 编译统计）。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |------|------|
//! | `abi` | 调用约定（ABI）：参数/返回值的寄存器与栈排布、栈帧布局（frame layout）、序言/尾声生成 |
//! | `block_order` | 按支配树 RPO 决定基本块的下降顺序（`BlockLoweringOrder`，含为边生成的块） |
//! | `div_magic` | 常数除法魔法数：Granlund–Montgomery 算法（Hacker's Delight 形式），后端共享的数论部分 |
//! | `emit` | 汇编文本发射：`AsmWriter` 组织函数头/块标签/序言尾声，顶层入口 `emit_vcode_assembly` |
//! | `emit_buffer` | 文本级发射缓冲（仿 Cranelift MachBuffer）：text 槽 + 符号化 Branch 槽，O(1) 分支优化 |
//! | `inst_predicate` | 下降时的 HIR 指令谓词：副作用 / 终结符 / 分支判定 |
//! | `libcall` | 运行时库调用（libcall）的枚举与符号名（目前仅 `memset`） |
//! | `lower` | 指令选择：`LowerContext` 驱动，后端实现 `LowerBackend` 逐条翻译 RaanaIR → VCode |
//! | `passes` | MIR 级 pass 基础设施：pre-RA / post-RA 两阶段管线，结构仿 `raana_ir::opt::pass` |
//! | `reg_alloc` | 寄存器分配：VReg → PReg / 栈槽，ION 回溯分配器（移植自 regalloc2） |
//! | `register` | 寄存器句柄：`Reg`（vreg/preg 双含义）、`Writable`（区分 def/use）、`VRegAllocator` |
//! | `stats` | 结构化编译统计（`CodegenStats` / `FunctionCodegenStats`） |
//! | `types` | 机器级类型系统：i32 / f32 / u64（地址）/ SIMD 向量，整数无符号语义 |
//! | `vcode` | VCode 容器与 trait 体系：`VCodeBuilder` / `VCodeContainer` / `MachInst` / `MachInstEmit` |
//!
//! ## 后端如何接入
//!
//! 后端（`anon_armv8` / `uika_riscv`）实现 `LowerBackend` trait：提供机器指令
//! 类型 `MInst`（实现 `MachInst` + `MachInstEmit`）、逐条 RaanaIR 指令的 lower
//! 规则、汇编伪指令与寄存器命名，并用 `mir_pipeline` 注册自己的 MIR pass；
//! 之后调用 `compile::<B>` 即可驱动完整流程。
//!
//! ## 详细文档
//!
//! - 寄存器分配子系统：见 `taki_mir/src/reg_alloc.rs`（G2/G3 中文文档）；
//! - VCode 容器与 trait 体系：见 `taki_mir/src/vcode.rs`（G2/G3 中文文档）。
//!
use core::fmt::Write;
use std::time::Instant;

use crate::{
    abi::{ABIMachineSpec, CalleeABI},
    block_order::BlockLoweringOrder,
    emit::AsmWriter,
    lower::{LowerBackend, LowerContext},
    reg_alloc::function::Function,
    stats::{CodegenStats, FunctionCodegenStats},
    vcode::{MachInst, MachInstEmit},
};

pub mod abi;
pub mod block_order;
pub mod div_magic;
pub mod emit;
pub mod emit_buffer;
pub mod inst_predicate;
pub mod lower;
pub mod passes;
pub mod reg_alloc;
pub mod register;
pub mod stats;
pub mod types;
pub mod vcode;

/// Allocation containers use this to reserve indexed storage without exposing
/// a particular backing collection implementation.
pub trait VecExt<T> {
    fn preallocate(&mut self, capacity: usize);
}

impl<T> VecExt<T> for Vec<T> {
    fn preallocate(&mut self, capacity: usize) {
        self.reserve(capacity.saturating_sub(self.capacity()));
    }
}

pub mod prelude {
    pub use raana_ir::ir::Program as HirProgram;
    pub use raana_ir::ir::Type as HirType;
    pub use raana_ir::ir::arena::Arena;
    pub use raana_ir::ir::builder_trait::*;
    pub use raana_ir::ir::inst_kind::*;
    pub use raana_ir::ir::{
        BasicBlock as HirBasicBlock, basic_block::BasicBlockData as HirBasicBlockData,
        layout::BasicBlockLayout as HirBasicBlockLayout,
    };
    pub use raana_ir::ir::{
        BinaryOp, Inst as HirInst, InstData as HirInstData, InstKind as HirInstKind,
    };
    pub use raana_ir::ir::{Function as HirFunction, FunctionData as HirFunctionData};

    pub use rustc_hash::FxHashSet;

    #[derive(Clone, Copy)]
    pub struct ArenaContext<'a> {
        pub program: &'a HirProgram,
        pub curr_func: Option<HirFunction>,
    }

    impl ArenaContext<'_> {
        /// Get current function data
        pub fn f(&self) -> &HirFunctionData {
            self.program.func_data(self.curr_func.unwrap())
        }

        pub fn set_current_function(&mut self, f: HirFunction) {
            self.curr_func.replace(f);
        }
    }

    impl Arena for ArenaContext<'_> {
        fn local(&self) -> &raana_ir::ir::arena::LocalArena {
            self.program
                .func_data(self.curr_func.unwrap())
                .local_arena()
        }

        fn local_mut(&mut self) -> &mut raana_ir::ir::arena::LocalArena {
            unimplemented!()
        }

        fn global(&self) -> &raana_ir::ir::arena::GlobalArena {
            self.program.global_arena()
        }

        fn global_mut(&mut self) -> &mut raana_ir::ir::arena::GlobalArena {
            unimplemented!()
        }
    }
}

use prelude::*;

pub enum GlobalData {
    I32(i32),
    F32(u32),
    ZeroInit(u32),
}

impl GlobalData {
    fn is_zero(&self) -> bool {
        match self {
            Self::I32(value) => *value == 0,
            Self::F32(bits) => *bits == 0,
            Self::ZeroInit(_) => true,
        }
    }

    fn size(&self) -> u32 {
        match self {
            Self::I32(_) | Self::F32(_) => 4,
            Self::ZeroInit(size) => *size,
        }
    }
}

fn lower_global_init(program: &HirProgram, init: HirInst) -> Vec<GlobalData> {
    let inst_data = program.inst_data(init);
    match inst_data.kind() {
        InstKind::Integer(i) => vec![GlobalData::I32(i.value())],
        InstKind::Float(f) => vec![GlobalData::F32(f.value().to_bits())],
        InstKind::ZeroInit => vec![GlobalData::ZeroInit(inst_data.ty().size() as u32)],
        InstKind::Aggregate(agg) => {
            let mut v = vec![];
            for &elem in agg.value() {
                v.extend(lower_global_init(program, elem));
            }
            v
        }
        _ => unreachable!(),
    }
}

pub struct CompileOutput {
    pub assembly: String,
    pub stats: CodegenStats,
}

/// Emit the backend's alignment pseudo-op (if any) before a global object of
/// `size` bytes. SIMD backends align large globals so vectorized global access
/// stays aligned; other backends return no directive and emit nothing.
fn emit_global_align<B: LowerBackend>(buf: &mut String, size: u32) {
    if size >= 16 {
        let directive = <<B::MInst as MachInst>::ABISpec as ABIMachineSpec>::global_align_directive();
        if let Some(directive) = directive {
            writeln!(buf, "{directive}").unwrap();
        }
    }
}

pub fn compile<B: LowerBackend>(p: &HirProgram) -> String
where
    B::MInst: MachInstEmit,
{
    compile_with_config::<B>(p, &B::CodegenConfig::default()).assembly
}

pub fn compile_with_config<B: LowerBackend>(
    p: &HirProgram,
    config: &B::CodegenConfig,
) -> CompileOutput
where
    B::MInst: MachInstEmit,
{
    let mut buf = String::new();
    let mut function_stats = Vec::new();

    let mut globals = vec![];
    for (gi, &inst) in p.global_inst_layout().iter().enumerate() {
        let data = p.inst_data(inst);
        let name = data.name().cloned().unwrap_or_else(|| format!("g_{}", gi));
        let InstKind::GlobalAlloc(alloc) = data.kind() else {
            unreachable!()
        };
        globals.push((name, lower_global_init(p, alloc.init())));
    }

    let (zero_initialized, initialized): (Vec<_>, Vec<_>) = globals
        .iter()
        .partition(|(_, data)| data.iter().all(GlobalData::is_zero));

    if !initialized.is_empty() {
        writeln!(buf, "{}", B::data_section_directive()).unwrap();
        for (name, data) in initialized {
            let size = data.iter().map(GlobalData::size).sum::<u32>();
            writeln!(buf, "{} {name}", B::global_directive()).unwrap();
            emit_global_align::<B>(&mut buf, size);
            writeln!(buf, "{name}:").unwrap();
            for entry in data {
                match entry {
                    GlobalData::I32(v) => {
                        writeln!(buf, "    {} {}", B::word_directive(), v).unwrap()
                    }
                    GlobalData::F32(bits) => {
                        writeln!(buf, "    {} {}", B::word_directive(), bits).unwrap()
                    }
                    GlobalData::ZeroInit(size) => {
                        writeln!(buf, "    {} {}", B::zero_directive(), size).unwrap()
                    }
                }
            }
            writeln!(buf).unwrap();
        }
    }

    if !zero_initialized.is_empty() {
        writeln!(buf, "{}", B::bss_section_directive()).unwrap();
        for (name, data) in zero_initialized {
            let size = data.iter().map(GlobalData::size).sum::<u32>();
            writeln!(buf, "{} {name}", B::global_directive()).unwrap();
            emit_global_align::<B>(&mut buf, size);
            writeln!(buf, "{name}:").unwrap();
            writeln!(buf, "    {} {size}", B::zero_directive()).unwrap();
            writeln!(buf).unwrap();
        }
    }

    writeln!(buf, "{}", B::text_section_directive()).unwrap();

    let pipeline = B::mir_pipeline(config);

    for &func in p.function_layout() {
        let func_data = p.func_data(func);
        if func_data.layout().entry_bb().is_none() {
            continue;
        }

        let mut stats = FunctionCodegenStats {
            function: func_data.name().to_string(),
            ..FunctionCodegenStats::default()
        };

        let arena = ArenaContext {
            program: p,
            curr_func: Some(func),
        };
        let lower_order = BlockLoweringOrder::new(arena);
        let abi = CalleeABI::new(arena);
        let lower = LowerContext::new(p, func, abi, lower_order);
        let mut vcode = lower.lower::<B>();
        #[cfg(debug_assertions)]
        vcode.verify("post-lowering").unwrap_or_else(|error| {
            log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
            panic!("function={} {error}", func_data.name());
        });

        if pipeline.run_pre_ra(&mut vcode, arena, &mut stats) {
            vcode.rebuild_operand_tables();
            #[cfg(debug_assertions)]
            vcode.verify("post-pre-RA-passes").unwrap_or_else(|error| {
                log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
                panic!("function={} {error}", func_data.name());
            });
        }

        let machine_env = vcode.abi.machine_env();
        let allocation_start = Instant::now();
        let output =
            crate::reg_alloc::ion::run(&vcode, machine_env).expect("register allocation failed");
        #[cfg(debug_assertions)]
        vcode.verify_alloc_output(&output).unwrap_or_else(|error| {
            log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
            panic!("function={} {error}", func_data.name());
        });
        let (reg_to_reg_edits, reg_to_stack_edits, stack_to_reg_edits, stack_to_stack_edits) =
            output.edits.iter().fold((0, 0, 0, 0), |counts, (_, edit)| {
                let crate::reg_alloc::reg::Edit::Move { from, to, .. } = edit;
                match (from.is_reg(), to.is_reg()) {
                    (true, true) => (counts.0 + 1, counts.1, counts.2, counts.3),
                    (true, false) => (counts.0, counts.1 + 1, counts.2, counts.3),
                    (false, true) => (counts.0, counts.1, counts.2 + 1, counts.3),
                    (false, false) => (counts.0, counts.1, counts.2, counts.3 + 1),
                }
            });
        stats.abi = vcode.abi.arg_stats();
        stats.regalloc = crate::stats::RegallocStats {
            spill_slots: output.num_spillslots as u64,
            reg_to_reg_edits,
            reg_to_stack_edits,
            stack_to_reg_edits,
        };
        log::debug!(target: "taki_mir::reg_alloc", "function={} allocation complete: locations={}, spill-slots={}, edits={}, allocation-time-us={}", func_data.name(), output.allocs.len(), output.num_spillslots, output.edits.len(), allocation_start.elapsed().as_micros());
        log::debug!(target: "taki_mir::reg_alloc", "function={} edit-kinds: reg-reg={}, reg-stack={}, stack-reg={}, stack-stack={}", func_data.name(), reg_to_reg_edits, reg_to_stack_edits, stack_to_reg_edits, stack_to_stack_edits);
        for (inst, allocs) in
            (0..vcode.num_insts()).map(|index| (index, output.inst_allocs(index as u32)))
        {
            log::trace!(target: "taki_mir::reg_alloc", "function={} inst={inst} allocations={allocs:?}", func_data.name());
        }
        for (point, edit) in &output.edits {
            log::trace!(target: "taki_mir::reg_alloc", "function={} edit at {point:?}: {edit:?}", func_data.name());
        }
        vcode.write_back_allocs(&output);
        #[cfg(debug_assertions)]
        vcode
            .verify("post-allocation-writeback")
            .unwrap_or_else(|error| {
                log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
                panic!("function={} {error}", func_data.name());
            });

        let spill_units = u32::try_from(output.num_spillslots).unwrap_or_else(|_| {
            panic!(
                "code generation invariant failed in function `{}`, phase `frame layout`: allocator spill-slot count exceeds frame range",
                func_data.name()
            )
        });
        let spill_size = spill_units
            .checked_mul(vcode.abi.spill_unit_bytes())
            .unwrap_or_else(|| {
                panic!(
                    "code generation invariant failed in function `{}`, phase `frame layout`: allocator spill area exceeds frame range",
                    func_data.name()
                )
            });
        vcode
            .abi
            .compute_frame_layout(spill_size, &output)
            .unwrap_or_else(|error| {
                panic!(
                    "code generation invariant failed in function `{}`, phase `frame layout`: {error}",
                    func_data.name()
                )
            });
        log::debug!(target: "taki_mir::reg_alloc", "function={} frame: spill-units={}, spill-bytes={}, frame-bytes={}", func_data.name(), output.num_spillslots, spill_size, vcode.abi.frame_layout().total_size);
        log::debug!(target: "taki_mir::emit", "function={} frame layout={:?}", func_data.name(), vcode.abi.frame_layout());

        // Materialize allocator edit-moves and legalize all pseudo-addressing
        // modes into the VCode. After this the `output` is fully consumed and
        // the emitter iterates the VCode directly.
        vcode.finalize_for_emission(&output);

        if pipeline.run_post_ra(&mut vcode, arena, &mut stats) {
            #[cfg(debug_assertions)]
            vcode.verify("post-post-RA-passes").unwrap_or_else(|error| {
                log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
                panic!("function={} {error}", func_data.name());
            });
        }

        let asm_start = buf.len();
        let mut w = AsmWriter::<B>::new(&mut buf, func_data, p, B::branch_opt_enabled(config));
        w.write_function(&vcode, &mut stats);
        let asm = &buf[asm_start..];
        log::debug!(target: "taki_mir::emit", "function={} final assembly: bytes={}, lines={}\n{}", func_data.name(), asm.len(), asm.lines().count(), asm);
        function_stats.push(stats);
    }

    if let Some(runtime) = B::runtime_assembly(p) {
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        buf.push('\n');
        buf.push_str(&runtime);
        if !runtime.ends_with('\n') {
            buf.push('\n');
        }
    }

    CompileOutput {
        assembly: buf,
        stats: CodegenStats::aggregate(function_stats),
    }
}
pub mod libcall;
