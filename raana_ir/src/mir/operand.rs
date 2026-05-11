pub enum Operand {
    Register(MirRegister, Size),
    ImmInt(i32),
    Memory(MemAddr),
}

#[derive(Debug, Clone, Copy)]
pub enum RegisterType {
    Int,
    Float,
}

#[derive(Debug, Clone, Copy)]
pub struct PhysRegister {
    rtype: RegisterType,
    id: u8,
}

#[derive(Debug, Clone, Copy)]
pub enum MirRegister {
    Virtual(u32),
    Physics(PhysRegister),
}

#[derive(Debug, Clone, Copy)]
pub enum Size {
    B32,
    B64,
}

#[derive(Debug, Clone, Copy)]
pub enum MemAddr {
    Base(MirRegister, Size),
    BaseOffset(MirRegister, i32),
    BaseIndexShift(MirRegister, MirRegister, u8),
    StackSlot(u32),
}

pub enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Mi,
    Pl,
}

#[allow(non_camel_case_types)]
pub enum Inst {
    mov {
        src: Operand,
        dst: Operand,
    },
    fmov {
        src: Operand,
        dst: Operand,
    },
    itf {
        dst: Operand,
        src: Operand,
    },
    fti {
        dst: Operand,
        src: Operand,
    },

    /// add dst, lhs, rhs
    add {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    /// sub dst, lhs, rhs
    sub {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    /// mul dst, lhs, rhs
    mul {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    /// sdiv dst, lhs, rhs
    sdiv {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },

    /// msub dst, sub, lhs, rhs  dst = sub - (lhs * rhs)
    msub {
        dst: Operand,
        sub: Operand,
        lhs: Operand,
        rhs: Operand,
    },

    fadd {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    fsub {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    fmul {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    fdiv {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },

    /// and dst, lhs, rhs
    and {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },

    /// orr dst, lhs, rhs
    orr {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    eor {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    lsl {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },
    asr {
        dst: Operand,
        lhs: Operand,
        rhs: Operand,
    },

    /// 比较指令 (cmp lhs, rhs) - 影响全局 NZCV 标志位，没有 dst！
    cmp {
        lhs: Operand,
        rhs: Operand,
    },
    /// 浮点比较指令 (fcmp lhs, rhs)
    fcmp {
        lhs: Operand,
        rhs: Operand,
    },
    /// 根据刚刚的比较结果，设置 dst 为 1 或 0 (cset dst, cond)
    /// SysY 常见模式: a < b -> Cmp(a, b), Cset(dst, Lt)
    cset {
        dst: Operand,
        cond: Cond,
    },

    /// 加载 (ldr dst, addr) -> 如果 dst 是 float 就是 ldr sX，否则 ldr wX/xX
    ldr {
        dst: Operand,
        addr: MemAddr,
    },
    /// 存储 (str src, addr)
    str {
        src: Operand,
        addr: MemAddr,
    },

    /// 无条件跳转 (b label)
    b {
        target: String,
    },
    /// 条件跳转 (b.cond label) - 依赖前面的 Cmp
    bcc {
        cond: Cond,
        target: String,
    },
    /// 函数调用 (bl func)。注意：要隐式地标记使用了哪些参数寄存器 (如 x0-x7)，
    /// 以便寄存器分配器知道它们会被覆盖！
    call {
        func: String,
        arg_regs: Vec<PhysRegister>,
    },
    /// ret
    ret,

    // ==========================================
    // 7. 伪指令 (Pseudo Instructions - 扩展性核心)
    // ==========================================
    /// 解决 Phi 节点和带参数 Jump 的相互覆盖问题！
    /// 寄存器分配器结束后，将其通过“拓扑排序”展开为安全的单步 Mov。
    ParallelCopy(Vec<(Operand, Operand)>),

    /// 加载全局变量的绝对地址 (adrp + add)
    /// ARMv8 要求分两步加载全局变量地址，在 MIR 中可以先用一条 Pseudo 指令表示，
    /// 等到最后生成汇编时再展开成两句。
    LoadGlobalAddr {
        dst: Operand,
        symbol: String,
    },

    /// 加载超过 12 bit 限制的大立即数 (movz + movk... 或 ldr =, 等)
    LoadLargeImm {
        dst: Operand,
        imm: i32,
    },
    GlobalInitI32 {
        init: Vec<i32>,
    },
    GlobalInitF32 {
        init: Vec<f32>,
    },
}
