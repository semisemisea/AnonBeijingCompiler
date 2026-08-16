//! # ABI：跨目标的调用约定与栈帧抽象
//!
//! 定位链：SysY 源码 → RaanaIR（平台无关 SSA，`raana_ir` crate）→ VCode（机器指令
//! 级，`taki_mir`）→ **ABI**（本模块：函数参数/返回值/栈帧的约定层）→ 汇编。同一份
//! 函数在 AArch64 上要遵守 AAPCS64、在 RISC-V 上要遵守其 psABI：前几个参数进物理
//! 寄存器、其余进调用方帧的 outgoing 区；返回值走约定寄存器；callee-saved 寄存器
//! 由被调方保存、caller-saved 由调用方保存；栈帧按 `stack_align` 对齐。这些规则因
//! 目标而异，本模块把它们抽象成泛型 ABI 层：**参数/返回值位置计算、栈帧布局、
//! 序言/尾声生成**对所有目标共用，只有目标特有策略由后端实现。
//!
//! ## 核心数据结构
//!
//! - `ABIMachineSpec` trait：目标 ABI 策略的完整清单——字长（`word_bits`）、栈
//!   对齐（`stack_align`）、溢出单位（`spillslot_size`/`spill_unit_bytes`）、
//!   callee-saved 判定（`is_callee_saved`）、物理寄存器环境（`get_machine_env`
//!   → `MachineEnv`）、栈加载/存储/常量/地址指令生成（`gen_load_stack`/
//!   `gen_store_stack`/`gen_load_imm`/`gen_load_addr`/`gen_move`）、参数位置计算
//!   （`compute_arg_loc`/`compute_call_arg_loc`）、帧建立/恢复与 clobber 保存
//!   （`gen_prologue_frame_setup`/`gen_epilogue_frame_restore`/
//!   `gen_clobber_save`/`gen_clobber_restore`）、帧相关寻址的发射前展开
//!   （`legalize_inst`）以及入口参数绑定伪指令（`gen_args`）。后端实现它即可
//!   接入全部共享流程。
//! - `CalleeABI`：**单个函数的 ABI 状态**，构造时只算参数位置，其余字段在
//!   lowering 过程中逐步填充。持有 `args`（每个参数的寄存器/栈位置）、栈槽分配
//!   （`allocate_stackslot`/`alloc_stackslot_or_get`）、outgoing 区尺寸
//!   （`set_outgoing_arg_size`）、是否含调用（`set_has_calls`）、入参绑定
//!   （`gen_copy_arg_to_reg`/`take_args`）、帧布局（`compute_frame_layout`/
//!   `frame_layout`）与序言/尾声（`gen_prologue`/`gen_epilogue`）。
//! - `ArgSlot`/`ArgRegBank`/`ArgLayoutPlanner`：参数位置规划。`ArgSlot` 表示一个
//!   参数落在寄存器还是入参栈区；`ArgLayoutPlanner::compute` 用分类闭包把标量
//!   参数分到 Int/Float/Vector 三个独立寄存器 bank（`ArgRegBank`），各组独立
//!   计数，寄存器用尽后溢出到栈，栈槽大小由 `stack_slot_size` 闭包决定——类型
//!   分类与槽大小是目标策略，溢出规划是共享逻辑。
//! - `StackAMode`：栈寻址三语义——`IncomingArg`（本帧参数区，即调用方帧的
//!   outgoing 区）、`Slot`（本帧栈对象区）、`OutgoingArg`（被调方帧的参数区，
//!   调用方写入实参用）。
//! - `FrameLayout`：帧布局求解结果：`callee_saved` 列表与
//!   `setup_area_size`/`clobber_size`/`spill_size`/`stackslots_size`/
//!   `outgoing_args_size`/`total_size` 各分区尺寸；spill 区位于 outgoing 区与
//!   栈对象区之后（`spill_base_bytes`/`spill_slot_offset`/`spill_region_end`）。
//! - 绑定对：`ArgPair`（入参 vreg ← 物理寄存器）、`CallArgPair`（调用实参）、
//!   `CallRetPair`（调用返回值）、`RetPair`（函数返回值）——lower 与后端指令
//!   之间传递"虚拟寄存器 ↔ 物理寄存器"的配对。
//!
//! ## 触发与使用场景
//!
//! - `taki_mir/src/lower.rs`：函数 lowering 开始时 `CalleeABI::new`（经
//!   `compute_arg_loc` 读函数签名算参数位置）；入口块 `gen_arg_setup` 逐参数调
//!   `gen_copy_arg_to_reg`——寄存器参数只记入 `ArgPair`（不发指令），栈参数发
//!   入参加载指令，值未被使用的参数调 `note_unused_register_arg` 跳过；最后
//!   `take_args` 打包成入口 `Args` 伪指令。调用点 lowering 经 `compute_call_arg_loc`
//!   计算实参位置与 outgoing 区尺寸（`precompute_outgoing_arg_size` 取全函数
//!   最大值）；**尾调用 lowering 经 `arg_slot` 把每个实参放到被调方会读取的
//!   位置**（AArch64：`anon_armv8/src/lower/call.rs`；RISC-V：`uika_riscv/src/
//!   lower.rs`）。
//! - `taki_mir/src/lib.rs`：寄存器分配结束后 `compute_frame_layout(spill_size,
//!   &output)` 由分配结果推导 callee-saved 集合并求解完整帧布局。
//! - `taki_mir/src/emit.rs`：发射阶段 `gen_prologue()`/`gen_epilogue()` 生成帧
//!   建立/恢复与 callee-saved 保存/恢复指令。
//!
//! ## 正确性要点
//!
//! - **callee-saved 集合必须含 allocator 编辑的两端**：`compute_frame_layout`
//!   除 `output.allocs` 外还收集 `output.edits` 中每条 `Move` 的 `from`/`to`——
//!   分配器处理 live-range 分裂时可能用 callee-saved 寄存器做寄存器间移动，该
//!   寄存器未必是任何指令的操作数；漏掉会让函数静默破坏调用方保存的寄存器。
//! - **寄存器参数零机器码绑定**：`Args` 伪指令在入口块把 vreg 定义为固定 ABI
//!   物理寄存器，不发射机器码，由寄存器分配解析固定定义——替代旧的"无条件存
//!   home slot 再加载"往返。
//! - **栈对象相对栈对象区寻址**：`allocate_stackslot` 的偏移相对栈对象区而非
//!   分配时刻的 outgoing 区（调用在 lowering 中陆续发现，折叠当时尺寸会让早期
//!   对象与后来的最大 outgoing 区重叠），帧布局统一在 `compute_frame_layout`
//!   求解。
//! - **溢出区位置与对齐**：spill 区在 outgoing 区与栈对象区之后；128 位向量占
//!   2 个 8 字节溢出单位，16 字节槽在 clobber 区内按 16 对齐，保证
//!   `str/ldr q` 不遇到未对齐地址。
//! - **setup 区**：只要函数含调用/入参栈参数/栈对象/callee-saved/spill 任一，
//!   就预留 `2 * word_bytes` 的 setup 区。
//! - 所有尺寸运算为 checked 算术，溢出以 `Result` 或带函数名与阶段的 panic
//!   暴露；入参统计（`crate::stats::AbiArgStats`）记录寄存器参数绑定/跳过/栈
//!   参数加载次数。
//!
//! ## 与后端及前端的衔接
//!
//! - 后端：`anon_armv8/src/abi.rs` 的 `AArch64Abi`（AAPCS64，整型 `x0-x7`、
//!   浮点 `d0-d7`、向量 `v0-v7`，见 `anon_armv8/src/regs.rs` 的 `INT_ARG_REGS`/
//!   `FLOAT_ARG_REGS`/`VECTOR_ARG_REGS`）与 `uika_riscv/src/abi.rs` 的
//!   `Riscv64ABI`（`a0-a7`/`fa0-fa7`，见 `uika_riscv/src/regs.rs` 的
//!   `ARG_REG`/`FARG_REG`）都实现 `ABIMachineSpec`；调用/返回/尾调用 lowering
//!   分别产出 `CallArgPair`/`CallRetPair`/`RetPair`/`TailCall` 指令。
//! - 前端：`CalleeABI::new` 经 `compute_arg_loc` 读 `ArenaContext` 中 RaanaIR
//!   函数 `params()` 的类型序列（`inst_data(param).ty()` → `HirType`），即
//!   raana_ir 函数签名 → ABI 参数位置的入口；返回值类型同样来自 `HirType`。
//! - 尾调用：TCO 保证调用者与被调者签名一致，故尾调用可复用本函数的入参槽
//!   （`arg_slot`）；发射端把尾声（帧恢复）拼接到 `TailCall` 前，跳转不链接。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`：独立寄存器 bank 布局（int/float 各自计数、溢出栈偏移
//!   与总尺寸）、目标栈槽尺寸策略、spill 区跟随 outgoing 与栈对象、越界 spill
//!   槽拒绝（`should_panic`）。
//! - `taki_mir/src/vcode/tests.rs` 的 `TestABI` 覆盖 `CalleeABI` 生命周期。
//! - 全量门禁：`cargo test -p taki_mir` 与 Docker harness（`make test` /
//!   `make test-riscv`，`-O 2`，见 `AGENTS.md`）。

use std::marker::PhantomData;
use std::num::NonZeroU64;

use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};
use tomori_utils::{PrimaryMap, SecondaryMap, entity_impl};

use crate::prelude::*;
use raana_ir::ir::TypeKind as HirTypeKind;
use crate::reg_alloc::reg::{MachineEnv, PReg, RegClass, SpillSlot};
use crate::register::{Reg, Writable};
use crate::types::LoweredType;
use crate::vcode::VCodeInst;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackAMode {
    /// Offset into the current frame's argument area.
    IncomingArg(i64, u32),
    /// Offset within the stack slots in the current frame.
    Slot(i64),
    /// Offset into the callee frame's argument area.
    OutgoingArg(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgSlot {
    Reg { reg: PReg, ty: HirType },
    Stack { offset: i64, ty: HirType },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgRegBank {
    Int,
    Float,
    /// 128-bit SIMD/vector bank (AAPCS64 NEON v0-v7).
    Vector,
}

/// Shared register-bank and stack-offset planning for scalar ABI arguments.
/// Type classification and overflow-slot sizing remain target policies.
pub struct ArgLayoutPlanner<'a> {
    int_regs: &'a [Reg],
    float_regs: &'a [Reg],
    vector_regs: &'a [Reg],
}

impl<'a> ArgLayoutPlanner<'a> {
    pub const fn new(int_regs: &'a [Reg], float_regs: &'a [Reg]) -> Self {
        Self {
            int_regs,
            float_regs,
            vector_regs: &[],
        }
    }

    pub const fn with_vector_regs(
        int_regs: &'a [Reg],
        float_regs: &'a [Reg],
        vector_regs: &'a [Reg],
    ) -> Self {
        Self {
            int_regs,
            float_regs,
            vector_regs,
        }
    }

    pub fn compute(
        &self,
        types: &[HirType],
        classify: impl Fn(&HirType) -> ArgRegBank,
        stack_slot_size: impl Fn(&HirType) -> u32,
    ) -> (Vec<ArgSlot>, u32) {
        let mut slots = Vec::with_capacity(types.len());
        // AAPCS64 allocates scalar-float and vector arguments from one shared
        // SIMD/FP register bank (`sN` is the low 32 bits of `vN`), so a
        // single `fp_index` advances across both. RISC-V never classifies a
        // value as `Vector`, so its separate F bank is unaffected.
        let (mut int_index, mut fp_index, mut stack_offset) = (0usize, 0usize, 0u32);

        for ty in types {
            let (reg, slot_index) = match classify(ty) {
                ArgRegBank::Int => (self.int_regs.get(int_index), &mut int_index),
                ArgRegBank::Float => (self.float_regs.get(fp_index), &mut fp_index),
                ArgRegBank::Vector => (self.vector_regs.get(fp_index), &mut fp_index),
            };
            let reg = reg.copied();
            *slot_index = slot_index
                .checked_add(1)
                .expect("argument register index overflow");

            if let Some(reg) = reg {
                slots.push(ArgSlot::Reg {
                    reg: reg
                        .to_physical_reg()
                        .expect("ABI argument register must be physical"),
                    ty: ty.clone(),
                });
            } else {
                slots.push(ArgSlot::Stack {
                    offset: i64::from(stack_offset),
                    ty: ty.clone(),
                });
                stack_offset = stack_offset
                    .checked_add(stack_slot_size(ty))
                    .expect("stack argument area overflow");
            }
        }

        (slots, stack_offset)
    }
}

impl StackAMode {
    fn offset_by(&self, offset: u32) -> Self {
        match self {
            StackAMode::IncomingArg(off, size) => {
                StackAMode::IncomingArg(off.checked_add(i64::from(offset)).unwrap(), *size)
            }
            StackAMode::Slot(off) => StackAMode::Slot(off.checked_add(i64::from(offset)).unwrap()),
            StackAMode::OutgoingArg(off) => {
                StackAMode::OutgoingArg(off.checked_add(i64::from(offset)).unwrap())
            }
        }
    }
}

pub trait ABIMachineSpec {
    type I: VCodeInst;

    fn word_bits() -> u32 {
        64
    }

    fn word_bytes() -> u32 {
        Self::word_bits() / 8
    }

    fn stack_align() -> u32;

    /// Required alignment (in bytes) of stack-allocated array objects.
    ///
    /// SIMD backends (e.g. AArch64 NEON) need array locals 16-byte aligned so
    /// vector loads/stores never see a misaligned base address; the shared
    /// `allocate_stackslot` rounds the array slot's absolute address (outgoing
    /// argument area + slot offset) up to this alignment. Non-SIMD backends
    /// inherit the default 8, which is a no-op because every stack slot size
    /// and the outgoing argument area are already 8-byte multiples.
    fn array_slot_align() -> u32 {
        8
    }

    /// Optional alignment pseudo-op emitted before a global object of at least
    /// 16 bytes. SIMD backends return e.g. `".p2align 4"` so vectorized global
    /// access stays 16-byte aligned; other backends return `None` and emit no
    /// directive.
    fn global_align_directive() -> Option<&'static str> {
        None
    }

    /// Number of logical allocator spill units needed by a value in `regclass`.
    ///
    /// `SpillSlot` values and `Output::num_spillslots` are expressed in these
    /// units. Frame layout converts units to bytes with `spill_unit_bytes()`.
    fn spillslot_size(_regclass: RegClass) -> u32;

    /// Physical byte width of one logical allocator spill unit.
    fn spill_unit_bytes() -> u32;

    fn is_callee_saved(_preg: PReg) -> bool;

    fn gen_load_stack(mem: StackAMode, dst: Writable<Reg>, ty: LoweredType) -> Self::I;

    /// Materialize the low bits of `value` according to `ty`.
    ///
    /// Integer constants are bit patterns, rather than host-sized signed values:
    /// this keeps i32 and pointer-width materialization distinct.
    fn gen_load_imm(dst: Writable<Reg>, value: u64, ty: LoweredType) -> Self::I;

    fn gen_load_addr(dst: Writable<Reg>, label: HirInst) -> Self::I;

    /// Generate a frame-dependent address for a stack location. `Slot` offsets
    /// are relative to the stack-object area and must be resolved after the
    /// final outgoing-argument area is known.
    fn gen_get_stack_addr(mem: StackAMode, dst: Writable<Reg>) -> Self::I;

    /// Build the function-entry pseudo-instruction that binds each register
    /// parameter virtual register to its fixed ABI physical register. It must
    /// emit no machine code; register allocation resolves the fixed defs.
    fn gen_args(args: Vec<ArgPair>) -> Self::I;

    fn gen_store_stack(src: Reg, mem: StackAMode, ty: LoweredType) -> Self::I;

    fn gen_spill_store(src: Reg, spill_off: i64, ty: LoweredType) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_store_stack(src, StackAMode::Slot(spill_off), ty)]
    }

    fn gen_spill_load(
        spill_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_load_stack(StackAMode::Slot(spill_off), dst, ty)]
    }

    /// Spill access for an allocator edit whose offset is already resolved
    /// against the post-prologue stack pointer.
    fn gen_spill_store_at_sp(src: Reg, spill_off: i64, ty: LoweredType) -> SmallVec<[Self::I; 4]> {
        Self::gen_spill_store(src, spill_off, ty)
    }

    fn gen_spill_load_at_sp(
        spill_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        Self::gen_spill_load(spill_off, dst, ty)
    }

    fn gen_incoming_arg_load(
        fp_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_load_stack(
            StackAMode::IncomingArg(fp_off, 0),
            dst,
            ty,
        )]
    }

    fn gen_move(src: Reg, dst: Reg, ty: LoweredType) -> Self::I;

    fn compute_arg_loc(arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32) {
        let types: Vec<_> = arena
            .f()
            .params()
            .iter()
            .map(|&param| arena.inst_data(param).ty().clone())
            .collect();
        Self::compute_call_arg_loc(&types)
    }

    /// Assign locations for a call signature using the same convention as
    /// incoming function parameters. The returned size includes ABI-required
    /// padding for the complete outgoing argument area.
    fn compute_call_arg_loc(types: &[HirType]) -> (Vec<ArgSlot>, u32);

    fn get_machine_env() -> &'static MachineEnv;

    fn gen_prologue_frame_setup(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_epilogue_frame_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    /// Expand frame-dependent pseudo addressing after allocation. Implementations
    /// must use only `MachineEnv::post_ra_scratch_by_class` registers.
    fn legalize_inst(_frame: &FrameLayout, inst: Self::I) -> SmallVec<[Self::I; 4]> {
        smallvec![inst]
    }

    /// Copy between allocator spill slots. Spill slots are eight-byte units, so
    /// an integer-width raw copy also preserves f32 values without requiring
    /// type information in allocator edits.
    fn gen_stack_to_stack_move(_from: i64, _to: i64) -> SmallVec<[Self::I; 4]> {
        panic!("target does not implement stack-to-stack allocator edits")
    }
}

#[derive(Debug, Clone)]
pub struct FrameLayout {
    pub callee_saved: Vec<PReg>,
    pub setup_area_size: u32,
    pub clobber_size: u32,
    pub spill_size: u32,
    pub stackslots_size: u32,
    pub outgoing_args_size: u32,
    pub total_size: u32,
}

impl FrameLayout {
    /// Allocator spills follow outgoing arguments and normal frame objects.
    pub fn spill_base_bytes(&self) -> u32 {
        self.outgoing_args_size
            .checked_add(self.stackslots_size)
            .expect("spill base overflow")
    }

    pub fn spill_slot_offset(&self, slot: SpillSlot, unit_bytes: u32) -> i64 {
        assert!(unit_bytes > 0, "spill unit size must be nonzero");
        let offset = (slot.raw_bits() as u64)
            .checked_mul(unit_bytes as u64)
            .and_then(|offset| offset.checked_add(self.spill_base_bytes() as u64))
            .expect("spill offset overflow");
        let end = offset
            .checked_add(unit_bytes as u64)
            .expect("spill access end overflow");
        assert!(
            end <= self.spill_region_end() as u64,
            "spill slot {slot} is outside the spill region"
        );
        i64::try_from(offset).expect("spill offset exceeds signed address range")
    }

    pub fn spill_region_end(&self) -> u32 {
        self.spill_base_bytes()
            .checked_add(self.spill_size)
            .expect("spill region overflow")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_alloc::reg::{PReg, RegClass};

    fn reg(index: usize, class: RegClass) -> Reg {
        Reg::from_physical_reg(PReg::new(index, class))
    }

    #[test]
    fn argument_layout_uses_independent_register_banks() {
        let int_regs = [reg(0, RegClass::Int), reg(1, RegClass::Int)];
        let float_regs = [reg(0, RegClass::Float)];
        let types = vec![
            HirType::get_i32(),
            HirType::get_f32(),
            HirType::get_i32(),
            HirType::get_f32(),
            HirType::get_i32(),
        ];
        let (slots, stack_size) = ArgLayoutPlanner::new(&int_regs, &float_regs).compute(
            &types,
            |ty| {
                if ty.is_f32() {
                    ArgRegBank::Float
                } else {
                    ArgRegBank::Int
                }
            },
            |_| 8,
        );

        assert!(matches!(slots[0], ArgSlot::Reg { .. }));
        assert!(matches!(slots[1], ArgSlot::Reg { .. }));
        assert!(matches!(slots[2], ArgSlot::Reg { .. }));
        assert!(matches!(slots[3], ArgSlot::Stack { offset: 0, .. }));
        assert!(matches!(slots[4], ArgSlot::Stack { offset: 8, .. }));
        assert_eq!(stack_size, 16);
    }

    #[test]
    fn argument_layout_preserves_target_stack_slot_sizes() {
        let types = vec![HirType::get_i32(), HirType::get_f32()];
        let planner = ArgLayoutPlanner::new(&[], &[]);
        let (_, fixed_size) = planner.compute(&types, |_| ArgRegBank::Int, |_| 8);
        let (sized_slots, sized_size) = planner.compute(
            &types,
            |_| ArgRegBank::Int,
            |ty| u32::try_from(ty.size()).unwrap(),
        );

        assert_eq!(fixed_size, 16);
        assert!(matches!(sized_slots[1], ArgSlot::Stack { offset: 4, .. }));
        assert_eq!(sized_size, 8);
    }

    #[test]
    fn spill_slots_follow_outgoing_and_local_stack_areas() {
        let frame = FrameLayout {
            callee_saved: vec![],
            setup_area_size: 16,
            clobber_size: 0,
            spill_size: 24,
            stackslots_size: 16,
            outgoing_args_size: 32,
            total_size: 96,
        };

        assert_eq!(frame.spill_base_bytes(), 48);
        assert_eq!(frame.spill_slot_offset(SpillSlot::new(0), 8), 48);
        assert_eq!(frame.spill_slot_offset(SpillSlot::new(2), 8), 64);
        assert_eq!(frame.spill_region_end(), 72);
    }

    #[test]
    #[should_panic(expected = "outside the spill region")]
    fn spill_slot_offset_rejects_out_of_range_slot() {
        let frame = FrameLayout {
            callee_saved: vec![],
            setup_area_size: 0,
            clobber_size: 0,
            spill_size: 8,
            stackslots_size: 0,
            outgoing_args_size: 0,
            total_size: 16,
        };

        frame.spill_slot_offset(SpillSlot::new(1), 8);
    }
}

#[derive(Debug, Clone)]
pub struct RetPair {
    pub vreg: Reg,
    pub preg: Reg,
}

#[derive(Debug, Clone)]
pub struct CallArgPair {
    pub vreg: Reg,
    pub preg: Reg,
}

#[derive(Debug, Clone)]
pub struct CallRetPair {
    pub vreg: Writable<Reg>,
    pub preg: Reg,
}

/// An incoming register function parameter bound directly to its ABI physical
/// register. The `Args` pseudo-instruction defines `vreg` from `preg` without
/// emitting any machine code, replacing the old unconditional home-slot
/// store/load round trip for register arguments.
#[derive(Debug, Clone)]
pub struct ArgPair {
    pub vreg: Writable<Reg>,
    pub preg: Reg,
}

/// The function abstraction at ABI level.
/// Most of the data is not initialized correctly at constructor.
/// It will gradually build during the lowering process.
pub struct CalleeABI<M: ABIMachineSpec> {
    args: Vec<ArgSlot>,

    /// Await for filling.
    total_stackslots_size: u32,

    /// Await for filling
    stackslots_offsets: PrimaryMap<StackSlot, u32>,

    sized_stack_arg_size: u32,

    alloc_to_ss: FxHashMap<HirInst, u32>,

    /// Await for filling
    outgoing_arg_size: u32,

    has_calls: bool,

    /// ?
    stackslots_keys: SecondaryMap<StackSlot, Option<StackSlotUniqueKey>>,

    frame_layout: Option<FrameLayout>,

    /// Register arguments awaiting `take_args`, which packages them into the
    /// entry `Args` pseudo. Populated by `gen_copy_arg_to_reg`; consumed by
    /// `gen_arg_setup` in lowering.
    reg_args: Vec<ArgPair>,

    /// Counters for incoming-argument statistics.
    bound_register_args: u64,
    unused_register_args_skipped: u64,
    incoming_stack_args_loaded: u64,

    _mach: PhantomData<M>,
}

impl<M: ABIMachineSpec> CalleeABI<M> {
    pub fn new(arena: ArenaContext<'_>) -> CalleeABI<M> {
        let (args, sized_stack_arg_size) = M::compute_arg_loc(arena);
        CalleeABI {
            args,
            total_stackslots_size: 0,
            sized_stack_arg_size,
            outgoing_arg_size: 0,
            has_calls: false,
            stackslots_offsets: PrimaryMap::new(),
            stackslots_keys: SecondaryMap::new(),
            alloc_to_ss: FxHashMap::default(),
            frame_layout: None,
            reg_args: Vec::new(),
            bound_register_args: 0,
            unused_register_args_skipped: 0,
            incoming_stack_args_loaded: 0,
            _mach: PhantomData,
        }
    }

    pub fn allocate_stackslot(&mut self, ty: HirType) -> u32 {
        let align = M::stack_align();
        // Stack objects are addressed relative to the stack-object area, not
        // to the outgoing area as it happened to be sized at allocation time.
        // Calls are discovered during lowering, so folding the then-current
        // outgoing size here would make early objects overlap a later maximum
        // outgoing-call area.
        let mut ret = self.total_stackslots_size;
        // Array objects are addressed at sp + outgoing_args_size + offset on
        // every backend. Pad the relative offset so the absolute address of an
        // array object is `array_slot_align()`-aligned (NEON `ldr/str q`
        // requires 16-byte alignment on AArch64). The outgoing area and every
        // slot size are 8-byte multiples, so this inserts at most 8 bytes of
        // padding and never overlaps a neighbouring slot. Non-SIMD backends
        // keep the default 8, making the rounding a no-op.
        if matches!(ty.kind(), HirTypeKind::Array(..)) {
            let slot_align = M::array_slot_align();
            let absolute = self.outgoing_arg_size.wrapping_add(ret);
            let pad = (slot_align.wrapping_sub(absolute % slot_align)) % slot_align;
            ret = ret.wrapping_add(pad);
        }
        let size = ty.size() as u32;
        let actual_size = size.next_multiple_of(align);
        self.total_stackslots_size = ret + actual_size;
        ret
    }

    pub fn alloc_stackslot_or_get(&mut self, alloc: HirInst, ty: HirType) -> u32 {
        if let Some(&offset) = self.alloc_to_ss.get(&alloc) {
            return offset;
        }
        let offset = self.allocate_stackslot(ty);
        self.alloc_to_ss.insert(alloc, offset);
        offset
    }

    pub fn set_outgoing_arg_size(&mut self, size: usize) {
        self.outgoing_arg_size = self.outgoing_arg_size.max(size as u32);
    }

    pub fn set_has_calls(&mut self) {
        self.has_calls = true;
    }

    pub fn machine_env(&self) -> &MachineEnv {
        M::get_machine_env()
    }

    pub fn spillslot_size(&self, regclass: RegClass) -> u32 {
        M::spillslot_size(regclass)
    }

    pub fn spill_unit_bytes(&self) -> u32 {
        M::spill_unit_bytes()
    }

    pub fn compute_frame_layout(
        &mut self,
        spill_size: u32,
        output: &crate::reg_alloc::reg::Output,
    ) -> Result<(), String> {
        // Allocator edits may use a callee-saved register even when no
        // instruction operand is assigned to it. This happens, for example,
        // when Ion resolves a live-range split with a register-to-register
        // move. Include both endpoints of every edit when deriving the
        // callee-save set; otherwise a function can silently clobber a
        // caller's preserved register.
        let mut callee_saved: Vec<PReg> = output
            .allocs
            .iter()
            .filter_map(|a| a.as_reg())
            .chain(output.edits.iter().flat_map(|(_, edit)| {
                let crate::reg_alloc::reg::Edit::Move { from, to, .. } = edit;
                [from.as_reg(), to.as_reg()].into_iter().flatten()
            }))
            .filter(|p| M::is_callee_saved(*p))
            .collect();
        callee_saved.sort_unstable();
        callee_saved.dedup();

        let stackslots_size = self.total_stackslots_size;
        let outgoing_args_size = self.outgoing_arg_size;
        // Callee-saved registers occupy their natural storage size (a 128-bit
        // vector is 2 spill units). 16-byte slots are aligned to 16 within the
        // clobber area so `str/ldr q` never sees a misaligned address.
        let clobber_size = callee_saved.iter().try_fold(0u32, |mut offset, p| {
            let size = M::spillslot_size(p.class())
                .checked_mul(M::spill_unit_bytes())
                .ok_or("callee-save slot size overflow")?;
            if size > M::word_bytes() {
                offset = offset
                    .checked_add(size - 1)
                    .ok_or("callee-save alignment overflow")?
                    & !(size - 1);
            }
            offset
                .checked_add(size)
                .ok_or("callee-save area exceeds frame range")
        })?;
        let setup_area_size = if self.has_calls
            || self.sized_stack_arg_size > 0
            || self.total_stackslots_size > 0
            || clobber_size > 0
            || spill_size > 0
        {
            2 * M::word_bytes()
        } else {
            0
        };
        let total = setup_area_size
            .checked_add(clobber_size)
            .and_then(|total| total.checked_add(spill_size))
            .and_then(|total| total.checked_add(stackslots_size))
            .and_then(|total| total.checked_add(outgoing_args_size))
            .ok_or("frame size exceeds frame range")?;
        let align = M::stack_align();
        let total = total
            .checked_add(align.checked_sub(1).ok_or("invalid stack alignment")?)
            .map(|size| size / align * align)
            .ok_or("aligned frame size exceeds frame range")?;
        let layout = FrameLayout {
            callee_saved,
            setup_area_size,
            clobber_size,
            spill_size,
            stackslots_size,
            outgoing_args_size,
            total_size: total,
        };
        self.frame_layout = Some(layout);
        Ok(())
    }

    /// Bind the `idx`-th function parameter to `into_reg`.
    ///
    /// Register arguments are recorded as [`ArgPair`]s to be packaged into the
    /// entry `Args` pseudo by [`CalleeABI::take_args`]; no instructions are
    /// emitted. Stack arguments are materialized with an incoming-argument
    /// load. Only called for parameters whose value is actually needed.
    pub fn gen_copy_arg_to_reg(&mut self, idx: usize, into_reg: Reg) -> SmallVec<[M::I; 4]> {
        let mut insts = smallvec![];
        match self.args[idx].clone() {
            ArgSlot::Reg { reg: preg, .. } => {
                self.bound_register_args += 1;
                self.reg_args.push(ArgPair {
                    vreg: Writable::from_reg(into_reg),
                    preg: preg.into(),
                });
            }
            ArgSlot::Stack { offset, ty } => {
                self.incoming_stack_args_loaded += 1;
                for inst in
                    M::gen_incoming_arg_load(offset, Writable::from_reg(into_reg), ty.into())
                {
                    insts.push(inst);
                }
            }
        }
        insts
    }

    /// Record a register parameter that was skipped because its value is
    /// never needed.
    pub fn note_unused_register_arg(&mut self) {
        self.unused_register_args_skipped += 1;
    }

    /// Incoming-argument binding statistics accumulated during lowering.
    pub fn arg_stats(&self) -> crate::stats::AbiArgStats {
        crate::stats::AbiArgStats {
            register_args_bound: self.bound_register_args,
            unused_register_args_skipped: self.unused_register_args_skipped,
            incoming_stack_args_loaded: self.incoming_stack_args_loaded,
        }
    }

    pub fn frame_layout(&self) -> &FrameLayout {
        self.frame_layout
            .as_ref()
            .expect("compute_frame_layout must be called before gen_prologue/gen_epilogue")
    }

    /// The ABI slot (register or incoming-stack) for the `idx`-th function
    /// parameter. Used by tail-call lowering to place each argument exactly
    /// where the callee will read it.
    pub fn arg_slot(&self, idx: usize) -> ArgSlot {
        self.args[idx].clone()
    }

    /// Drain the collected register-argument bindings and package them into the
    /// entry `Args` pseudo. Returns `None` when no register argument was bound.
    pub fn take_args(&mut self) -> Option<M::I> {
        if self.reg_args.is_empty() {
            None
        } else {
            Some(M::gen_args(core::mem::take(&mut self.reg_args)))
        }
    }

    pub fn gen_prologue(&self) -> SmallVec<[M::I; 16]> {
        let frame = self.frame_layout();
        let mut insts = smallvec![];
        insts.extend(M::gen_prologue_frame_setup(frame));
        insts.extend(M::gen_clobber_save(frame));
        insts
    }

    pub fn gen_epilogue(&self) -> SmallVec<[M::I; 16]> {
        let frame = self.frame_layout();
        let mut insts = smallvec![];
        insts.extend(M::gen_clobber_restore(frame));
        insts.extend(M::gen_epilogue_frame_restore(frame));
        insts
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StackSlot(u32);
entity_impl!(StackSlot, "ss");

pub struct StackSlotData {
    pub size: u32,
    pub align_lg2: u8,
    pub key: Option<StackSlotUniqueKey>,
}

#[derive(Debug, Clone)]
pub struct StackSlotUniqueKey(NonZeroU64);

impl StackSlotUniqueKey {
    fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).unwrap())
    }

    fn bits(self) -> u64 {
        self.0.get()
    }
}
