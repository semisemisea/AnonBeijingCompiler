use crate::reg_alloc::index::{Block, Inst, InstRange};
use crate::reg_alloc::reg::{Operand, PRegSet, RegClass, VReg};

// Function trait：客户端（后端生成的 VCode）必须实现的"函数视图"。
// 寄存器分配器（ion/ 下的算法）只通过这个 trait 读取函数体，不关心其具体表示：
// 它把函数抽象成 CFG（基本块 + 前驱/后继边）、块参数（本项目用 block 参数实现
// Phi，见 glossary）、以及每条指令的操作数 / 破坏寄存器列表。
// 在分配流程中的角色：为分配器提供遍历函数结构、查询每个 vreg 的定义/使用
// 位置与约束所需的全部信息；实现方必须保证返回的切片与句柄严格对应——
// Inst 与 Block 都是 arena 下标句柄（见 glossary），索引必须合法。
pub trait Function {
    // 指令总数（所有块之和），分配器据此预分配按指令索引的数组。
    fn num_insts(&self) -> usize;
    // 基本块总数，分配器据此预分配按块索引的数组。
    fn num_blocks(&self) -> usize;
    // 函数的入口块：CFG 遍历与活性分析的起点。
    fn entry_block(&self) -> Block;
    // 返回 block 内指令的半开区间 [first, last)，用于按序访问块内指令。
    fn block_insns(&self, block: Block) -> InstRange;
    // block 的所有后继块（CFG 出边目标）。顺序即出边序号：branch_blockparams
    // 的 succ_idx 按此切片下标，两者必须一致。
    fn block_succs(&self, block: Block) -> &[Block];
    // block 的所有前驱块（CFG 入边来源），用于反向遍历（如活性传播）。
    fn block_preds(&self, block: Block) -> &[Block];
    // block 的形参 vreg 切片（即本项目的 Phi）：每条入边跳转进来时随边携带的
    // 实参值最终落到这里的形参上，见 branch_blockparams 与 glossary 的"Block 参数"。
    fn block_params(&self, block: Block) -> &[VReg];
    // insn 是否是返回指令（函数出口的终结符），分配器据此识别函数结束点。
    fn is_ret(&self, insn: Inst) -> bool;
    // insn 是否是分支/跳转指令（块末尾的终结符，控制流在此离开本块），
    // 分配器据此定位块的最后一条指令并处理其出边实参。
    fn is_branch(&self, insn: Inst) -> bool;
    // 块 block 中终结符 insn 的第 succ_idx 条出边（下标对应 block_succs）上
    // 携带的实参 vreg 切片，随边传给目标块，与目标块的 block_params 一一对应。
    fn branch_blockparams(&self, block: Block, insn: Inst, succ_idx: usize) -> &[VReg];
    // insn 的完整操作数列表：每个 Operand 描述一个 vreg 的寄存器类别、使用/定义
    // 属性、位置（early/late）与分配约束（constraint），是分配决策的主要输入。
    fn inst_operands(&self, insn: Inst) -> &[Operand];
    // insn 执行后会破坏的物理寄存器集合——这些 PReg 被写入但不作为指令的 vreg
    // 输出（如调用指令破坏的调用者保存寄存器、内联汇编的 clobber 列表）。
    // 分配器必须避免把跨指令存活的 vreg 安排在其中的 PReg 上。
    fn inst_clobbers(&self, insn: Inst) -> PRegSet;
    // 虚拟寄存器总数：vreg 编号取 0..num_vregs()，分配器据此预分配 vreg 状态数组。
    fn num_vregs(&self) -> usize;
    // 某一寄存器类的一个溢出色（spill slot）占多少字节，决定栈帧溢出区的布局。
    fn spillslot_size(&self, regclass: RegClass) -> usize;
    // 可选钩子：为 true 时，占用多个溢出色的值用"最后一个色"的编号命名；
    // 默认 false（用第一个色命名）。客户端可按后端溢出惯例覆写。
    fn multi_spillslot_named_by_last_slot(&self) -> bool {
        false
    }
    // 可选钩子：为 true 时允许一条指令定义多个 vreg；默认 false。
    // 默认分配器假设每条指令至多一个定义，多输出指令的后端需覆写此方法。
    fn allow_multiple_vreg_defs(&self) -> bool {
        false
    }
}
