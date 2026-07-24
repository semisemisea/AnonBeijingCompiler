use core::fmt::Write;

use crate::{
    abi::CalleeABI,
    block_order::BlockLoweringOrder,
    emit::AsmWriter,
    lower::{LowerBackend, LowerContext},
    reg_alloc::function::Function,
    vcode::MachInstEmit,
};

pub mod abi;
pub mod block_order;
pub mod emit;
pub mod inst_predicate;
pub mod lower;
pub mod reg_alloc;
pub mod register;
pub mod riscv64;
pub mod types;
pub mod vcode;

pub mod prelude {
    use std::collections::HashSet;

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

    use rustc_hash::FxBuildHasher;

    #[derive(Clone, Copy)]
    pub struct ArenaContext<'a> {
        pub program: &'a HirProgram,
        pub curr_func: Option<HirFunction>,
    }

    pub type FxHashSet<K> = HashSet<K, FxBuildHasher>;

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

pub fn compile<B: LowerBackend>(p: &HirProgram) -> Result<String, crate::lower::CodegenError>
where
    B::MInst: MachInstEmit,
{
    let mut buf = String::new();

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
            writeln!(buf, "{} {name}", B::global_directive()).unwrap();
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
            writeln!(buf, "{} {name}", B::global_directive()).unwrap();
            writeln!(buf, "{name}:").unwrap();
            let size = data.iter().map(GlobalData::size).sum::<u32>();
            writeln!(buf, "    {} {size}", B::zero_directive()).unwrap();
            writeln!(buf).unwrap();
        }
    }

    writeln!(buf, "{}", B::text_section_directive()).unwrap();

    for &func in p.function_layout() {
        let func_data = p.func_data(func);
        if func_data.layout().entry_bb().is_none() {
            continue;
        }

        let arena = ArenaContext {
            program: p,
            curr_func: Some(func),
        };
        let lower_order = BlockLoweringOrder::new(arena);
        let abi = CalleeABI::new(arena);
        let lower = LowerContext::new(p, func, abi, lower_order)?;
        let mut vcode = lower.lower::<B>()?;
        vcode.verify("post-lowering").unwrap_or_else(|error| {
            log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
            panic!("function={} {error}", func_data.name());
        });

        let machine_env = vcode.abi.machine_env();
        let output =
            crate::reg_alloc::alloc::run(&vcode, machine_env).expect("register allocation failed");
        vcode.verify_alloc_output(&output).unwrap_or_else(|error| {
            log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
            panic!("function={} {error}", func_data.name());
        });
        log::debug!(target: "taki_mir::reg_alloc", "function={} allocation complete: locations={}, spill-slots={}, edits={}", func_data.name(), output.allocs.len(), output.num_spillslots, output.edits.len());
        for (inst, allocs) in
            (0..vcode.num_insts()).map(|index| (index, output.inst_allocs(index as u32)))
        {
            log::trace!(target: "taki_mir::reg_alloc", "function={} inst={inst} allocations={allocs:?}", func_data.name());
        }
        for (point, edit) in &output.edits {
            log::debug!(target: "taki_mir::reg_alloc", "function={} edit at {point:?}: {edit:?}", func_data.name());
        }
        vcode.write_back_allocs(&output);
        vcode
            .verify("post-allocation-writeback")
            .unwrap_or_else(|error| {
                log::error!(target: "taki_mir::verify", "function={} {error}", func_data.name());
                panic!("function={} {error}", func_data.name());
            });

        let spill_units = u32::try_from(output.num_spillslots).map_err(|_| {
            crate::lower::CodegenError::backend(
                arena,
                "frame layout",
                "allocator spill-slot count exceeds frame range",
            )
        })?;
        let spill_size = spill_units
            .checked_mul(vcode.abi.spill_unit_bytes())
            .ok_or_else(|| {
                crate::lower::CodegenError::backend(
                    arena,
                    "frame layout",
                    "allocator spill area exceeds frame range",
                )
            })?;
        vcode.abi.compute_frame_layout(spill_size, &output);
        log::debug!(target: "taki_mir::emit", "function={} frame layout={:?}", func_data.name(), vcode.abi.frame_layout());

        let asm_start = buf.len();
        let mut w = AsmWriter::<B>::new(&mut buf, func_data, p);
        w.write_function(&vcode, &output);
        let asm = &buf[asm_start..];
        log::debug!(target: "taki_mir::emit", "function={} final assembly: bytes={}, lines={}\n{}", func_data.name(), asm.len(), asm.lines().count(), asm);
    }

    Ok(buf)
}
