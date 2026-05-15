use crate::mir::prelude::*;

#[derive(Debug, Clone, Copy)]
pub struct Register(u32);

impl Register {
    pub fn new_virtual(id: u32) -> Register {
        assert!(id > 64);
        Register(id)
    }

    pub fn new_physics(id: u32) -> Register {
        assert!(id <= 64);
        Register(id)
    }

    pub fn is_virtual(&self) -> bool {
        self.0 > 64
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Value {
    Register(Register, Size),
    I32(i32),
    F32(f32),
}

#[derive(Debug, Clone, Copy)]
pub enum Size {
    B32,
    B64,
}

impl From<usize> for Size {
    fn from(t: usize) -> Size {
        match t {
            32 => Self::B32,
            64 => Self::B64,
            _ => panic!("Not supported bit!"),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum MemAddr {
    Base(Register, Size),
    BaseOffset(Register, i32),
    BaseIndexShift(Register, Register, u8),
    StackSlot(u32),
    Global(HirInst),
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
        src: Register,
        dst: Value,
    },
    fmov {
        src: Value,
        dst: Value,
    },
    itf {
        dst: Value,
        src: Value,
    },
    fti {
        dst: Value,
        src: Value,
    },

    /// add dst, lhs, rhs
    add {
        dst: Register,
        lhs: Register,
        rhs: Value,
    },
    /// sub dst, lhs, rhs
    sub {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    /// mul dst, lhs, rhs
    mul {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    /// sdiv dst, lhs, rhs
    sdiv {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },

    /// msub dst, sub, lhs, rhs  dst = sub - (lhs * rhs)
    msub {
        dst: Value,
        sub: Value,
        lhs: Value,
        rhs: Value,
    },

    fadd {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    fsub {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    fmul {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    fdiv {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },

    /// and dst, lhs, rhs
    and {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },

    /// orr dst, lhs, rhs
    orr {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    eor {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    lsl {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },
    asr {
        dst: Value,
        lhs: Value,
        rhs: Value,
    },

    /// 比较指令 (cmp lhs, rhs) - 影响全局 NZCV 标志位，没有 dst！
    cmp {
        lhs: Value,
        rhs: Value,
    },
    /// 浮点比较指令 (fcmp lhs, rhs)
    fcmp {
        lhs: Value,
        rhs: Value,
    },
    /// 根据刚刚的比较结果，设置 dst 为 1 或 0 (cset dst, cond)
    /// SysY 常见模式: a < b -> Cmp(a, b), Cset(dst, Lt)
    cset {
        dst: Value,
        cond: Cond,
    },

    /// 加载 (ldr dst, addr) -> 如果 dst 是 float 就是 ldr sX，否则 ldr wX/xX
    ldr {
        dst: Value,
        addr: MemAddr,
    },
    /// 存储 (str src, addr)
    str {
        src: Value,
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
        arg_regs: Vec<Register>,
    },
    /// ret
    ret,

    // ==========================================
    // 7. 伪指令 (Pseudo Instructions - 扩展性核心)
    // ==========================================
    /// 解决 Phi 节点和带参数 Jump 的相互覆盖问题！
    /// 寄存器分配器结束后，将其通过“拓扑排序”展开为安全的单步 Mov。
    ParallelCopy(Vec<(Value, Value)>),

    /// 加载全局变量的绝对地址 (adrp + add)
    /// ARMv8 要求分两步加载全局变量地址，在 MIR 中可以先用一条 Pseudo 指令表示，
    /// 等到最后生成汇编时再展开成两句。
    LoadGlobalAddr {
        dst: Value,
        symbol: String,
    },

    /// 加载超过 12 bit 限制的大立即数 (movz + movk... 或 ldr =, 等)
    LoadLargeImm {
        dst: Value,
        imm: i32,
    },
    GlobalInitI32 {
        init: Vec<i32>,
    },
    GlobalInitF32 {
        init: Vec<f32>,
    },
}
