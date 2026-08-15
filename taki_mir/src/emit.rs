//! # 汇编文本发射（AsmWriter 与顶层入口）
//!
//! 把寄存器分配完成、`finalize_for_emission` 之后的 [`VCodeContainer`] 翻译成
//! 汇编文本。真正的指令打印由每条指令的 [`MachInstEmit::emit`] 完成，本模块
//! 负责**组织**：函数头/块标签/序言尾声的顺序、EmitBuffer 的装配与渲染。
//!
//! ## 结构
//!
//! - [`AsmWriter`]：一次函数发射的写入器。持有输出串、函数数据、程序与
//!   branch_opt 开关；[`write_function`](AsmWriter::write_function) 是主流程：
//!   写全局指令与函数标签 → 建 [`EmitBuffer`](crate::emit_buffer::EmitBuffer)
//!   → 逐块发射（prologue/body/epilogue）→ buffer `finish()` 渲染回输出串。
//! - [`emit_vcode_assembly`]：**顶层入口**——给定程序/函数/已 finalize 的
//!   VCode，返回整段汇编文本。后端可直接用它发射手工构造的机器层程序
//!   （如显式 SIMD VCode 验证）。
//! - `emit_legalized`：发射前先跑 `ABISpec::legalize_inst` 做**伪寻址展开**——
//!   把分配后依赖栈帧的伪寻址指令（如 `StackAMode` 伪指令）展开成实际
//!   sp 偏移的指令（`FrameLayout` 已知），再逐条 emit。实现见
//!   `anon_armv8/src/abi.rs` 的 `legalize_inst`。
//!
//! ## 与 emit_buffer 的分工
//!
//! 指令文本先以 **text slot** 形式进 [`EmitBuffer`](crate::emit_buffer::EmitBuffer)
//! （每条指令一个槽，固定 4 字节宽度），分支则作为符号化 `Branch` 槽保存
//! 目标（`MirBlockIndex`），由 buffer 在 `finish()` 时统一做标签解析与
//! 分支优化（截断/取反/改写）。详见 [`emit_buffer`](crate::emit_buffer) 模块文档。

use core::fmt::Write;

use crate::abi::{ABIMachineSpec, FrameLayout};
use crate::block_order::MirBlockIndex;
use crate::emit_buffer::EmitBuffer;
use crate::lower::LowerBackend;
use crate::prelude::*;
use crate::stats::FunctionCodegenStats;
use crate::vcode::{EmitContext, MachInst, MachInstEmit, VCodeContainer, VCodeInst};

pub(crate) struct AsmWriter<'a, B: LowerBackend> {
    pub buf: &'a mut String,
    pub func_data: &'a HirFunctionData,
    pub program: &'a HirProgram,
    branch_opt: bool,
    pub(crate) _phantom: std::marker::PhantomData<B>,
}

impl<'a, B: LowerBackend> AsmWriter<'a, B> {
    pub fn new(
        buf: &'a mut String,
        func_data: &'a HirFunctionData,
        program: &'a HirProgram,
        branch_opt: bool,
    ) -> AsmWriter<'a, B> {
        Self {
            buf,
            func_data,
            program,
            branch_opt,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<B: LowerBackend> AsmWriter<'_, B> {
    /// Emit one function through the [`EmitBuffer`]: every instruction
    /// (prologue, blocks, epilogue) lands in the buffer as a text slot, then
    /// the finished buffer is rendered into the output string.
    pub fn write_function(
        &mut self,
        vcode: &VCodeContainer<B::MInst>,
        stats: &mut FunctionCodegenStats,
    ) where
        B::MInst: MachInstEmit,
    {
        let name = self.func_data.name();
        writeln!(self.buf, "{} {name}", B::global_directive()).unwrap();
        writeln!(self.buf, "{name}:").unwrap();

        let frame = vcode.abi.frame_layout();
        let block_labels: Vec<String> = vcode
            .block_order()
            .lowered_order()
            .iter()
            .map(|lb| B::format_block_label(lb, self.func_data))
            .collect();
        let mut buffer =
            EmitBuffer::<B>::new(self.program, name.to_owned(), block_labels, self.branch_opt);

        for inst in &vcode.abi.gen_prologue() {
            emit_legalized::<B::MInst, B>(frame, inst, &mut buffer);
        }

        for (bi, _lb) in vcode.block_order().lowered_order().iter().enumerate() {
            buffer.bind_label(MirBlockIndex::new(bi));
            for inst in vcode.block_insts(bi) {
                if inst.needs_epilogue() {
                    for epi in &vcode.abi.gen_epilogue() {
                        emit_legalized::<B::MInst, B>(frame, epi, &mut buffer);
                    }
                }
                inst.emit(&mut buffer).unwrap();
                buffer.end_inst().unwrap();
            }
        }

        buffer.optimize_branches();
        buffer.resolve();
        let branch_stats = buffer.branch_stats();
        log::debug!(
            target: "taki_mir::emit",
            "function={} branch-opt: ran={} changed={} fallthrough={} inverted={} threaded={} dead={} veneers={}",
            name,
            branch_stats.ran,
            branch_stats.changed,
            branch_stats.fallthrough_removed,
            branch_stats.branches_inverted,
            branch_stats.labels_threaded,
            branch_stats.dead_jumps_removed,
            branch_stats.veneers_inserted,
        );
        stats.branch_opt = branch_stats;
        self.buf.push_str(&buffer.finish());
        writeln!(self.buf).unwrap();
    }
}

/// Legalize and emit an ABI-generated instruction (prologue, epilogue).
/// VCode instructions are already finalized and go through the buffer's
/// plain `inst.emit` path instead.
fn emit_legalized<I, B>(frame: &FrameLayout, inst: &I, buffer: &mut EmitBuffer<'_, B>)
where
    I: VCodeInst + MachInstEmit,
    B: LowerBackend<MInst = I>,
{
    for inst in I::ABISpec::legalize_inst(frame, inst.clone()) {
        inst.emit(buffer).unwrap();
        buffer.end_inst().unwrap();
    }
}

/// Emit a finalized [`VCodeContainer`] (post register allocation and
/// `finalize_for_emission`) as textual assembly for one function. Exposed so a
/// backend can validate machine-layer programs that are constructed directly
/// (e.g. explicit SIMD VCode) rather than lowered from HIR.
pub fn emit_vcode_assembly<B: LowerBackend>(
    program: &HirProgram,
    func_data: &HirFunctionData,
    vcode: &VCodeContainer<B::MInst>,
) -> String
where
    B::MInst: MachInstEmit,
{
    let mut buf = String::new();
    let mut stats = FunctionCodegenStats::default();
    let mut writer = AsmWriter::<B>::new(
        &mut buf,
        func_data,
        program,
        B::branch_opt_enabled(&B::CodegenConfig::default()),
    );
    writer.write_function(vcode, &mut stats);
    buf
}
