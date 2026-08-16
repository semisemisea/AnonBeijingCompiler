//! Complete purity / memory-effect analysis (interprocedural).
//!
//! Two cooperating fixpoints over the call graph:
//!
//! 1. **Points-to sets** (top-down): for every function parameter, the set
//!    of concrete objects it may point to — globals, stack objects (tagged
//!    with their defining function), or `Unknown`. Flattened to concrete
//!    objects only, so recursive/cyclic call graphs converge.
//! 2. **Effect summaries** (bottom-up): per function, the set of objects it
//!    may read / write (in its own terms: globals, its own allocs, `Param(i)`
//!    symbols), plus unknown-provenance and stdin/stdout flags.
//!
//! The summaries answer the classic purity questions:
//! - `is_pure`: no writes to memory or stdout → movable/duplicable;
//! - `is_removable`: additionally no stdin reads → deletable when unused;
//! - `may_write_memory`: a memory barrier for load hoisting / CSE.
//!
//! SysY's restricted memory model (no pointers, no heap) makes the base
//! object of every access decidable (`memory::BaseEnv`); the interprocedural
//! `alias` here refines the intra-procedural rules for argument pairs using
//! the points-to sets (see `docs/memory_alias_analysis.md` §3-§4).
//!
//! The sysylib boundary (`soyo_compiler/src/frontend/utils.rs`) is modeled
//! explicitly: scalar I/O reads stdin / writes stdout; `getarray`/`putarray`
//! read/write their array argument; timing functions touch neither program
//! memory nor program-visible state beyond I/O.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 一句话定位：这是 raana_ir 的**过程间纯度 / 副作用分析**——给定整个 `Program`，
//! 回答「一个函数能不能被移动 / 复制 / 删除 / 当作内存屏障」。它是 LICM 外提纯调用、
//! DCE 删无用调用、GVN 跨调用 CSE、DSE 删死存储、IPSCCP 跨调用失效的公共地基
//! （见「使用方清单」）。文中术语（固定点、pass、SSA 等）见
//! `docs/offline-handbook/glossary.md`。
//!
//! 与姊妹分析的定位区分：`pure_function.rs` 是早期、函数粒度的保守纯度判定；
//! 本模块输出**效果摘要**（读写对象集合 + points-to + stdin/stdout 标志），粒度更
//! 细、更精确，实际 pass 用的是 `EffectAnalysis`。过程内部分（基址 + 常量偏移解析、
//! pointer-slot 模式）来自 `memory.rs` 的 `BaseEnv`，本模块在其上叠加过程间信息。
//!
//! ## 数据结构
//!
//! 三个「内存对象」枚举，对应三种视角：
//!
//! - `AbstractObject`：**具体对象**（points-to 集合的元素，全局视角）。`Global(Inst)`
//!   是全局对象（`GlobalAlloc` 指令）；`Alloc(Function, Inst)` 是某函数的栈对象
//!   （`Alloc` 指令，带定义函数，因为不同函数的局部指令下标会重复）；`Unknown`
//!   表示未知来源——可能是任何东西。注意集合**只含具体对象、不含符号化别名**，
//!   这正是递归 / 循环调用图能收敛的关键（见「算法」）。
//! - `EffectObject`：**函数视角的效应目标**（摘要的元素）。`Global(Inst)`、
//!   `Alloc(Function, Inst)`（该函数自己的栈帧时就是摘要的 owner）、
//!   `Param(usize)`（本函数第 `index` 个参数）。
//! - `WriteRoot`：**调用者视角的写根**（被调函数可能写到、且调用者看得见的东西）。
//!   `Global(Inst)` 或 `Local(Function, Inst)`（调用者 `func` 的栈对象）。被调函数
//!   自己的栈帧对调用者不可见，不会出现在这里。
//!
//! `FunctionEffects` 是单个函数的效应摘要：`reads` / `writes` 是可能读 / 写的
//! `EffectObject` 集合；`reads_unknown` / `writes_unknown` 表示经未知来源地址读 /
//! 写（`Unknown` provenance 出现过的痕迹）；`reads_io` / `writes_io` 表示读 stdin /
//! 写 stdout——I/O 是可观察行为，与内存读写不同，不能自由删除。
//!
//! `EffectAnalysis` 是分析结果本体，一次 `Pass::run` 构建一份、之后只读，内部三张
//! 表：`envs`（每函数的 `BaseEnv`）、`effects`（每函数的 `FunctionEffects`）、
//! `points_to`（`(函数, 参数下标) → 具体对象集合`）。
//!
//! ## API 详解
//!
//! ### 构造
//!
//! - `new(program: &Program) -> EffectAnalysis`：全程序构建。对每个函数：声明函数
//!   （`is_decl()`，无 body）直接查 `decl_effects` 拿固定摘要，查不到就按最坏情况
//!   （读写 unknown + 读写 I/O）；有 body 的函数建 `BaseEnv`、扫一遍 `Load` /
//!   `Store` / `MemZero` 得到直接效应（`direct_effects`），并把每条 `Call` /
//!   `TailCall` 登记成调用点 `CallSiteInfo`（caller、callee、实参）。最后依次跑
//!   两个不动点：`propagate_points_to`（top-down）→ `propagate_effects`
//!   （bottom-up）。
//!
//! ### 查询 API（全部 `&self` 只读）
//!
//! - `env_of(&self, func) -> &BaseEnv`：取出 `func` 的过程内基址环境，供 pass 复用
//!   （DSE / IPSCCP 直接用它做基址 + 常量偏移解析）。
//! - `effects_of(&self, func) -> &FunctionEffects`：`func` 的最终效应摘要（含所有
//!   传递进来的被调函数效应）。
//! - `is_pure(&self, func) -> bool`：摘要层面「不写内存、不写 I/O」——函数可以被
//!   自由移动、复制，结果未使用时可删除（读内存不在此列：读是纯的）。
//! - `is_removable(&self, func) -> bool`：`is_pure` 且不读 stdin——没有任何可观察
//!   效应，结果未使用时可删除。这是 DCE / LICM 判「这个 call 能不能删」的唯一
//!   依据。
//! - `points_to_of(&self, func, index) -> Option<&HashSet<AbstractObject>>`：`func`
//!   第 `index` 个参数的 points-to 集合；没有任何调用点贡献对象时返回 `None`
//!   （此时按空集处理）。
//! - `targets_of<A: Arena + ?Sized>(&self, arena, func, addr) -> Option<HashSet<AbstractObject>>`：
//!   `func` 内地址 `addr`（一条指令）指向的具体对象集合。`None` 表示未知来源
//!   （可能指向任何东西）。实现：先 `base_of` 求基址——`Alloc` / `Global` 直接返回
//!   单例集合；`Param(i)` 查 points-to 表，含 `Unknown` 则返回 `None`；`Unknown`
//!   返回 `None`。
//! - `alias<A: Arena + ?Sized>(&self, arena, func, a, b) -> AliasResult`：过程间
//!   别名判定，用 points-to 精化 `memory.rs` 的过程内规则。参数 vs 参数（下标
//!   不同）：两集合互不相交且都不含 `Unknown` → `NoAlias`（集合缺省视为空），否则
//!   `MayAlias`；参数 vs 全局 / 栈对象：集合不含 `Unknown` 且不含该对象 →
//!   `NoAlias`，否则 `MayAlias`；其余组合回退 `env.alias_with_bases`（过程内规则）。
//! - `call_may_write(&self, callee, targets: Option<&HashSet<AbstractObject>>) -> bool`：
//!   调用 `callee` 是否可能写 `targets` 里的任何对象。`targets == None` 表示被问的
//!   地址可能是任何东西（此时退化为 `may_write_memory`）。内部把 callee 的
//!   `writes` 逐项与 `targets` 比对，`Param(j)` 用 callee 的 points-to 展开，任何
//!   一步遇到 `Unknown` 一律回答「会写」。
//! - `call_may_read(&self, callee, targets: Option<&HashSet<AbstractObject>>) -> bool`：
//!   同上的读版本。
//! - `call_write_roots(&self, callee, func) -> Option<Vec<WriteRoot>>`：调用
//!   `callee` 可能写到的、`func` 视角下的根集合。`None` 表示 callee 可能写任何
//!   东西（`writes_unknown` 为真，或参数展开遇到 `Unknown`）。callee 自己栈帧上的
//!   对象对 `func` 不可见，省略。
//! - `call_read_roots(&self, callee, func) -> Option<Vec<WriteRoot>>`：同上的读
//!   版本。
//!
//! ### 私有部件（了解即可）
//!
//! - `decl_effects(name: &str) -> Option<FunctionEffects>`：sysylib 声明的固定摘要
//!   （见「正确性 / 边界」），未知声明返回 `None`。
//! - `direct_effects(program, func)`：函数体内**不含调用**的直接效应——只认 `Load`
//!   （读）、`Store` / `MemZero`（写），地址经 `base_of` 分类成 `EffectObject` 或
//!   unknown。
//! - `classify_actual(env, ctx, points_to, func, arg)`：把调用点实参 `arg` 分类成
//!   具体对象集合（调用方语境）。实参是 `Param(i)` 时查**调用方自己**的 points-to
//!   表——这就是 points-to 沿调用图传递的通道。
//! - `apply_effect_object(obj, callee, points_to, out, out_unknown)`：把 callee 的
//!   一个效应对象代入 caller 语境：全局照抄；callee 自己的栈帧丢弃；`Param(j)`
//!   按 callee 的 points-to 展开成具体对象。
//! - `merge_effects(dst, delta) -> bool`：把 delta 并进 dst，返回是否有新增内容。
//!
//! ## 算法：两个不动点如何协作
//!
//! `new` 里按固定顺序跑两个不动点，各自循环到「一轮没有任何变化」为止：
//!
//! 1. **top-down points-to**（`propagate_points_to`）：从调用点实参出发，把
//!    `(callee, 参数下标) → 对象` 逐层传递。每个调用点对每个实参调
//!    `classify_actual`（实参是 `Param` 时读调用方当前积累的集合），结果加入被调方
//!    的集合。每轮**先收集全部 additions 再统一写入**——分类读的是本轮已有的
//!    集合，写入发生在收集之后，避免同一轮内读写顺序造成的不一致。任一集合插入了
//!    新对象就 `changed = true`，继续下一轮。
//! 2. **bottom-up effects**（`propagate_effects`）：把被调方的摘要沿调用边向上汇总
//!    进调用方。每个调用点：克隆 callee 摘要，对其 `reads` / `writes` 逐项
//!    `apply_effect_object`（参数按**已经稳定**的 points-to 展开成具体对象），再
//!    OR 上 unknown / I/O 标志，得到 delta；全部调用点算完后用 `merge_effects`
//!    把 delta 并进各调用方，有变化就再来一轮。
//!
//! **协作关系**：points-to 先跑（它的信息只来自实参与调用方自身，不依赖摘要），
//! 跑稳之后 effects 才能把 `Param(j)` 翻译成具体对象——顺序不能反。
//!
//! **收敛条件**：两个不动点的数据流方向都是「集合单调增长 + 有界」。points-to 的
//! 元素是具体对象（`Global` / `Alloc` / `Unknown`），最多是整个程序里的对象总数，
//! 传播不会产生新对象；effects 的元素同理（`EffectObject` 有限），外加四个 bool
//! 标志。单调 + 有限 ⇒ 最多迭代「对象总数」轮必然到达不动点，不会振荡。
//!
//! ## 使用方清单
//!
//! （以下为 `raana_ir/src` 内 grep 实证的使用点；行号随版本漂移，仅供参考。）
//!
//! - `opt/passes/licm.rs`：每次 `Pass::run` 重建 `EffectAnalysis`（fixpoint 每轮
//!   重建，约 780-807 行）。三处核心用法：① 纯调用外提——`is_removable` 判定
//!   callee 可外提（434 行）；② store / MemZero 冲突——`targets_of` 求写目标后问
//!   `call_may_read`（441-446 行）；③ 兄弟 call 冲突——`call_read_roots` 取读根
//!   集合，再问 `call_may_write`（450-463 行，读根未知时退化为
//!   `may_write_memory`）。load 外提安全性（`load_hoist_safe`，738-769 行）：
//!   `alias` 判 store 冲突，`targets_of` + `call_may_write` 判 call 冲突。
//! - `opt/passes/dce.rs`：`EffectAnalysis::new`（239 行），对结果未使用的 `Call`
//!   用 `is_removable(call.callee())` 决定是否删除（247 行）。
//! - `opt/passes/gvn.rs`：每个 `run_on` 构建一次（459 行）。call 的 CSE 用
//!   `is_removable`（258 行）；store / MemZero 后按 `targets_of` + `call_may_read`
//!   淘汰可能读到该目标的 callee（527-536 行）；`effects_of(c).may_write_memory()`
//!   作跨调用写屏障（542 行）；兄弟 call 用 `call_read_roots` + `call_may_write`
//!   保持执行顺序（548-580 行，`WriteRoot` ↔ `AbstractObject` 手工换算）。
//! - `opt/passes/dse.rs`：`EffectAnalysis::new`（116 行）；`env_of` 复用基址解析
//!   （163 行）；`call_write_roots(...).is_some()` 判定调用是否为写屏障（203 行）；
//!   删死存储时用 `call_write_roots` / `call_read_roots` / `call_may_read` 保守保留
//!   可能被调用读 / 写的 cell（251、415-440 行）。
//! - `opt/passes/ipsccp.rs`：`targets_of` 判定 load 的地址是否为已知全局，是则直接
//!   取值（370 行）；调用后用 `call_write_roots` 失效被写的对象（427、437 行）；
//!   `env_of` 复用基址解析（623、745、773 行）。
//! - `opt/analysis_passes/pure_function.rs`：姊妹分析（函数粒度、保守），文档互相
//!   引用，不直接调用本模块。
//!
//! ## 正确性 / 边界
//!
//! - **递归 / 循环调用图**：两个不动点都单调有界（见「算法」），自递归、互递归、
//!   循环调用都会收敛；`recursion_converges_conservatively` 测试直接验证。
//! - **Unknown provenance**：地址 `base_of` 不到任何对象（如经指针槽二次取址后的
//!   值）时，效应记为 `reads_unknown` / `writes_unknown`；`is_pure` /
//!   `is_removable` 立即为假；`call_may_write` / `call_may_read` 恒答「可能」；
//!   `targets_of` / `call_write_roots` / `call_read_roots` 返回 `None`——全部保守。
//! - **sysylib 边界**（`soyo_compiler/src/frontend/utils.rs` 注入的声明）：
//!   `getint` / `getch` / `getfloat` 只读 stdin；`getarray` / `getfarray` 读 stdin
//!   并写参数 0；`putint` / `putch` / `putfloat` / `putf` 只写 stdout；
//!   `putarray` / `putfarray` 写 stdout 并读参数 0；`_sysy_starttime` /
//!   `_sysy_stoptime` 读写 I/O（计时可观察，但不碰程序内存）；其余未知声明按最坏
//!   情况处理。注意这些固定摘要在 `decl_effects` 里**按名字匹配**，与调用点无关。
//! - **callee 私有栈帧**：被调函数自己 `Alloc` 的对象对调用者不可见——效应传播
//!   （`apply_effect_object`）与根查询（`call_write_roots` / `call_read_roots`）都
//!   会丢弃，避免把函数私有内存误算成调用者的效应。
//! - **快照语义**：`EffectAnalysis` 是一次性构建、之后只读的快照，反映构建时 IR
//!   的调用关系与内存操作；任何 pass 增删调用 / 存储指令后都必须重建（LICM 在
//!   fixpoint 每轮重建即是此意）。
//! - **声明函数**：无 body 的函数（`is_decl()`）没有直接效应，只有固定摘要或最坏
//!   假设；它们没有 body 可扫，也不会产生调用点。
//! - **尾调用**：`TailCall` 与 `Call` 同等对待——都登记调用点、都参与传播。
//!
//! ## 验证
//!
//! 本文件 `mod tests` 共 12 个单元测试，覆盖：无内存操作纯函数（`is_pure` +
//! `is_removable`）、写全局不纯、读全局仍纯、callee 效应传递到 caller、参数写在
//! 调用点替换成实参对象、`getint` 的 I/O 与不可删除、递归自调用收敛、points-to
//! 精化参数 vs 全局别名、不相交参数不别名、同一对象传两次仍可别名、`getarray` 写
//! 实参数组、局部栈对象永不与参数别名。各使用方 pass 的测试（licm / dce / gvn /
//! dse / ipsccp 的 `mod tests`）从行为上间接回归本分析；全量
//! `cargo test -p raana_ir` 是最终门禁。
//!
use rustc_hash::FxHashMap as HashMap;
use rustc_hash::FxHashSet as HashSet;

use crate::{
    ir::{Function, Inst, InstKind, Program, arena::Arena},
    opt::{
        analysis_passes::memory::{AliasResult, BaseEnv, MemObject},
        pass::ArenaContext,
    },
};

/// A concrete abstract memory object used in points-to sets and effect
/// summaries. `Alloc` objects carry their defining function because local
/// instruction ids repeat across functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbstractObject {
    /// A global object.
    Global(Inst),
    /// A stack object of the given function.
    Alloc(Function, Inst),
    /// Unknown provenance: may be anything.
    Unknown,
}

/// An effect target in a function's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectObject {
    /// A global object.
    Global(Inst),
    /// A stack object of the given function (its own frame when the
    /// function is the summary's owner).
    Alloc(Function, Inst),
    /// The function's parameter at this index.
    Param(usize),
}

/// A concrete root a callee may write, in the caller's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteRoot {
    /// A global object.
    Global(Inst),
    /// A stack object of the caller function.
    Local(Function, Inst),
}

/// Memory / I/O effects of one function.
#[derive(Debug, Clone, Default)]
pub struct FunctionEffects {
    /// Objects the function may read.
    pub reads: HashSet<EffectObject>,
    /// Objects the function may write.
    pub writes: HashSet<EffectObject>,
    /// Reads through an unknown-provenance address.
    pub reads_unknown: bool,
    /// Writes through an unknown-provenance address.
    pub writes_unknown: bool,
    /// Reads stdin (consumes input; observable).
    pub reads_io: bool,
    /// Writes stdout (observable).
    pub writes_io: bool,
}

impl FunctionEffects {
    /// No writes to memory or I/O: the function may be freely moved,
    /// duplicated, or deleted when its result is unused.
    pub fn is_pure(&self) -> bool {
        self.writes.is_empty() && !self.writes_unknown && !self.writes_io
    }

    /// No observable effects at all (may still read memory): removable when
    /// its result is unused.
    pub fn is_removable(&self) -> bool {
        self.is_pure() && !self.reads_io
    }

    /// The function may write program memory (through known or unknown
    /// addresses): a barrier for load hoisting / load CSE.
    pub fn may_write_memory(&self) -> bool {
        !self.writes.is_empty() || self.writes_unknown
    }

    /// The function may read program memory (through known or unknown
    /// addresses).
    pub fn may_read_memory(&self) -> bool {
        !self.reads.is_empty() || self.reads_unknown
    }
}

/// A call site: caller, callee, and the actual arguments (positionally
/// matching the callee's parameters).
struct CallSiteInfo {
    caller: Function,
    callee: Function,
    args: Vec<Inst>,
}

/// Whole-program effect analysis: per-function base environments, points-to
/// sets, and effect summaries. Built once per `Pass::run`; immutable
/// afterwards.
pub struct EffectAnalysis {
    envs: HashMap<Function, BaseEnv>,
    effects: HashMap<Function, FunctionEffects>,
    points_to: HashMap<(Function, usize), HashSet<AbstractObject>>,
}

/// Fixed summaries for the sysylib declarations (see
/// `soyo_compiler/src/frontend/utils.rs`). `None` for unknown declarations.
fn decl_effects(name: &str) -> Option<FunctionEffects> {
    let mut fx = FunctionEffects::default();
    match name {
        "getint" | "getch" | "getfloat" => {
            fx.reads_io = true;
        }
        "getarray" | "getfarray" => {
            fx.reads_io = true;
            fx.writes.insert(EffectObject::Param(0));
        }
        "putint" | "putch" | "putfloat" | "putf" => {
            fx.writes_io = true;
        }
        "putarray" | "putfarray" => {
            fx.writes_io = true;
            fx.reads.insert(EffectObject::Param(0));
        }
        "_sysy_starttime" | "_sysy_stoptime" => {
            fx.reads_io = true;
            fx.writes_io = true;
        }
        _ => return None,
    }
    Some(fx)
}

impl EffectAnalysis {
    pub fn new(program: &Program) -> EffectAnalysis {
        let mut envs = HashMap::default();
        let mut effects = HashMap::default();
        let mut call_sites: Vec<CallSiteInfo> = Vec::new();

        for &func in program.function_layout() {
            let data = program.func_data(func);
            if data.layout().is_decl() {
                let fx = decl_effects(data.name()).unwrap_or_else(|| {
                    // Unknown declaration: assume the worst.
                    FunctionEffects {
                        reads_unknown: true,
                        writes_unknown: true,
                        reads_io: true,
                        writes_io: true,
                        ..Default::default()
                    }
                });
                effects.insert(func, fx);
                continue;
            }
            let mut env = BaseEnv::new(data);
            let env_ctx = ArenaContext {
                program,
                curr_func: Some(func),
            };
            env.build_tables(&env_ctx, func);
            envs.insert(func, env);
            effects.insert(func, direct_effects(program, func));

            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    match data.inst_data(inst).kind() {
                        InstKind::Call(call) => call_sites.push(CallSiteInfo {
                            caller: func,
                            callee: call.callee(),
                            args: call.args().to_vec(),
                        }),
                        InstKind::TailCall(tail_call) => call_sites.push(CallSiteInfo {
                            caller: func,
                            callee: tail_call.callee(),
                            args: tail_call.args().to_vec(),
                        }),
                        _ => {}
                    }
                }
            }
        }

        let mut analysis = EffectAnalysis {
            envs,
            effects,
            points_to: HashMap::default(),
        };
        analysis.propagate_points_to(program, &call_sites);
        analysis.propagate_effects(program, &call_sites);
        analysis
    }

    pub fn env_of(&self, func: Function) -> &BaseEnv {
        &self.envs[&func]
    }

    pub fn effects_of(&self, func: Function) -> &FunctionEffects {
        &self.effects[&func]
    }

    pub fn is_pure(&self, func: Function) -> bool {
        self.effects_of(func).is_pure()
    }

    /// Whether a call to `func` can be removed when its result is unused.
    pub fn is_removable(&self, func: Function) -> bool {
        self.effects_of(func).is_removable()
    }

    /// The points-to set of `func`'s parameter `index`, if any call site
    /// contributes objects to it.
    pub fn points_to_of(&self, func: Function, index: usize) -> Option<&HashSet<AbstractObject>> {
        self.points_to.get(&(func, index))
    }

    /// The set of concrete abstract objects `addr` (inside `func`) may point
    /// to. `None` means unknown provenance (may point to anything).
    pub fn targets_of<A: Arena + ?Sized>(
        &self,
        arena: &A,
        func: Function,
        addr: Inst,
    ) -> Option<HashSet<AbstractObject>> {
        let base = self.env_of(func).base_of(arena, addr);
        match base {
            MemObject::Alloc(a) => Some(HashSet::from_iter([AbstractObject::Alloc(func, a)])),
            MemObject::Global(g) => Some(HashSet::from_iter([AbstractObject::Global(g)])),
            MemObject::Param(i) => {
                let set = self.points_to.get(&(func, i));
                match set {
                    Some(set) if !set.contains(&AbstractObject::Unknown) => Some(set.clone()),
                    _ => None,
                }
            }
            MemObject::Unknown => None,
        }
    }

    /// Whether a call to `callee` may write to any object in `targets`.
    /// `targets == None` means the queried address may be anything.
    pub fn call_may_write(
        &self,
        callee: Function,
        targets: Option<&HashSet<AbstractObject>>,
    ) -> bool {
        let fx = self.effects_of(callee);
        if fx.writes_unknown {
            return true;
        }
        let Some(targets) = targets else {
            return fx.may_write_memory();
        };
        fx.writes.iter().any(|w| match w {
            EffectObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
            EffectObject::Alloc(cf, a) => targets.contains(&AbstractObject::Alloc(*cf, *a)),
            EffectObject::Param(j) => {
                self.points_to
                    .get(&(callee, *j))
                    .into_iter()
                    .flatten()
                    .any(|o| match o {
                        AbstractObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
                        AbstractObject::Alloc(cf, a) => {
                            targets.contains(&AbstractObject::Alloc(*cf, *a))
                        }
                        AbstractObject::Unknown => true,
                    })
            }
        })
    }

    /// The roots (in `func`'s own terms) a call to `callee` may write.
    /// `None` means the callee may write anything (unknown writes or
    /// unknown parameter targets). Callee-frame allocs are invisible to
    /// `func` and omitted.
    pub fn call_write_roots(&self, callee: Function, func: Function) -> Option<Vec<WriteRoot>> {
        let fx = self.effects_of(callee);
        if fx.writes_unknown {
            return None;
        }
        let mut roots = Vec::new();
        for w in &fx.writes {
            match w {
                EffectObject::Global(g) => roots.push(WriteRoot::Global(*g)),
                EffectObject::Alloc(cf, a) if *cf == func => {
                    roots.push(WriteRoot::Local(func, *a));
                }
                EffectObject::Alloc(..) => {}
                EffectObject::Param(j) => {
                    for o in self.points_to.get(&(callee, *j)).into_iter().flatten() {
                        match o {
                            AbstractObject::Global(g) => roots.push(WriteRoot::Global(*g)),
                            AbstractObject::Alloc(cf, a) if *cf == func => {
                                roots.push(WriteRoot::Local(func, *a));
                            }
                            AbstractObject::Alloc(..) => {}
                            AbstractObject::Unknown => return None,
                        }
                    }
                }
            }
        }
        Some(roots)
    }

    /// Whether a call to `callee` may read any object in `targets`.
    /// `targets == None` means the queried address may be anything.
    pub fn call_may_read(
        &self,
        callee: Function,
        targets: Option<&HashSet<AbstractObject>>,
    ) -> bool {
        let fx = self.effects_of(callee);
        if fx.reads_unknown {
            return true;
        }
        let Some(targets) = targets else {
            return fx.may_read_memory();
        };
        fx.reads.iter().any(|r| match r {
            EffectObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
            EffectObject::Alloc(cf, a) => targets.contains(&AbstractObject::Alloc(*cf, *a)),
            EffectObject::Param(j) => {
                self.points_to
                    .get(&(callee, *j))
                    .into_iter()
                    .flatten()
                    .any(|o| match o {
                        AbstractObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
                        AbstractObject::Alloc(cf, a) => {
                            targets.contains(&AbstractObject::Alloc(*cf, *a))
                        }
                        AbstractObject::Unknown => true,
                    })
            }
        })
    }

    /// The roots (in `func`'s own terms) a call to `callee` may read.
    /// `None` means the callee may read anything. Callee-frame allocs are
    /// invisible to `func` and omitted.
    pub fn call_read_roots(&self, callee: Function, func: Function) -> Option<Vec<WriteRoot>> {
        let fx = self.effects_of(callee);
        if fx.reads_unknown {
            return None;
        }
        let mut roots = Vec::new();
        for r in &fx.reads {
            match r {
                EffectObject::Global(g) => roots.push(WriteRoot::Global(*g)),
                EffectObject::Alloc(cf, a) if *cf == func => {
                    roots.push(WriteRoot::Local(func, *a));
                }
                EffectObject::Alloc(..) => {}
                EffectObject::Param(j) => {
                    for o in self.points_to.get(&(callee, *j)).into_iter().flatten() {
                        match o {
                            AbstractObject::Global(g) => roots.push(WriteRoot::Global(*g)),
                            AbstractObject::Alloc(cf, a) if *cf == func => {
                                roots.push(WriteRoot::Local(func, *a));
                            }
                            AbstractObject::Alloc(..) => {}
                            AbstractObject::Unknown => return None,
                        }
                    }
                }
            }
        }
        Some(roots)
    }

    /// Interprocedural alias result between two addresses inside `func`,
    /// refining the intra-procedural rules with the points-to sets.
    pub fn alias<A: Arena + ?Sized>(
        &self,
        arena: &A,
        func: Function,
        a: Inst,
        b: Inst,
    ) -> AliasResult {
        let env = self.env_of(func);
        let base_a = env.base_of(arena, a);
        let base_b = env.base_of(arena, b);
        use MemObject::*;
        match (base_a, base_b) {
            (Param(i), Param(j)) if i != j => {
                // Disjoint points-to sets (an absent set is empty) and no
                // unknown provenance prove the parameters never alias.
                let sa = self.points_to.get(&(func, i));
                let sb = self.points_to.get(&(func, j));
                let disjoint = match (sa, sb) {
                    (Some(sa), Some(sb)) => {
                        !sa.contains(&AbstractObject::Unknown)
                            && !sb.contains(&AbstractObject::Unknown)
                            && sa.intersection(sb).next().is_none()
                    }
                    _ => true,
                };
                if disjoint {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            (Param(i), Global(g)) | (Global(g), Param(i)) => {
                let s = self.points_to.get(&(func, i));
                let no_alias = match s {
                    Some(s) => {
                        !s.contains(&AbstractObject::Unknown)
                            && !s.contains(&AbstractObject::Global(g))
                    }
                    None => true,
                };
                if no_alias {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            (Param(i), Alloc(a)) | (Alloc(a), Param(i)) => {
                let s = self.points_to.get(&(func, i));
                let no_alias = match s {
                    Some(s) => {
                        !s.contains(&AbstractObject::Unknown)
                            && !s.contains(&AbstractObject::Alloc(func, a))
                    }
                    None => true,
                };
                if no_alias {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            _ => env.alias_with_bases(arena, a, b, base_a, base_b),
        }
    }

    /// Top-down points-to propagation over the call graph, to fixpoint.
    fn propagate_points_to(&mut self, program: &Program, call_sites: &[CallSiteInfo]) {
        loop {
            let mut changed = false;
            // Collect the contributions first: classification reads the
            // current sets while the application writes them.
            let mut additions: Vec<((Function, usize), AbstractObject)> = Vec::new();
            for site in call_sites {
                let env = &self.envs[&site.caller];
                let ctx = ArenaContext {
                    program,
                    curr_func: Some(site.caller),
                };
                for (index, &arg) in site.args.iter().enumerate() {
                    for obj in classify_actual(env, &ctx, &self.points_to, site.caller, arg) {
                        additions.push(((site.callee, index), obj));
                    }
                }
            }
            for (key, obj) in additions {
                changed |= self.points_to.entry(key).or_default().insert(obj);
            }
            if !changed {
                return;
            }
        }
    }

    /// Bottom-up effect propagation over the call graph, to fixpoint.
    fn propagate_effects(&mut self, program: &Program, call_sites: &[CallSiteInfo]) {
        loop {
            let mut changed = false;
            let mut deltas: HashMap<Function, FunctionEffects> = HashMap::default();
            for site in call_sites {
                let callee_fx = self.effects_of(site.callee).clone();
                let mut delta = FunctionEffects::default();
                for &obj in &callee_fx.reads {
                    apply_effect_object(
                        obj,
                        site.callee,
                        &self.points_to,
                        &mut delta.reads,
                        &mut delta.reads_unknown,
                    );
                }
                for &obj in &callee_fx.writes {
                    apply_effect_object(
                        obj,
                        site.callee,
                        &self.points_to,
                        &mut delta.writes,
                        &mut delta.writes_unknown,
                    );
                }
                delta.reads_unknown |= callee_fx.reads_unknown;
                delta.writes_unknown |= callee_fx.writes_unknown;
                delta.reads_io |= callee_fx.reads_io;
                delta.writes_io |= callee_fx.writes_io;
                *deltas.entry(site.caller).or_default() |= delta;
            }
            for (func, delta) in deltas {
                changed |= merge_effects(self.effects.get_mut(&func).unwrap(), delta);
            }
            let _ = program;
            if !changed {
                return;
            }
        }
    }
}

/// Substitute one effect object of the callee into caller terms, using the
/// callee's points-to sets. The callee's own frame (`Alloc(callee, ·)`) is
/// invisible to the caller and dropped.
fn apply_effect_object(
    obj: EffectObject,
    callee: Function,
    points_to: &HashMap<(Function, usize), HashSet<AbstractObject>>,
    out: &mut HashSet<EffectObject>,
    out_unknown: &mut bool,
) {
    match obj {
        EffectObject::Global(g) => {
            out.insert(EffectObject::Global(g));
        }
        EffectObject::Alloc(cf, _a) if cf == callee => {}
        EffectObject::Alloc(cf, a) => {
            out.insert(EffectObject::Alloc(cf, a));
        }
        EffectObject::Param(j) => {
            let set = points_to.get(&(callee, j));
            let Some(set) = set else { return };
            for o in set {
                match o {
                    AbstractObject::Global(g) => {
                        out.insert(EffectObject::Global(*g));
                    }
                    AbstractObject::Alloc(cf, _a) if *cf == callee => {}
                    AbstractObject::Alloc(cf, a) => {
                        out.insert(EffectObject::Alloc(*cf, *a));
                    }
                    AbstractObject::Unknown => *out_unknown = true,
                }
            }
        }
    }
}

fn merge_effects(dst: &mut FunctionEffects, delta: FunctionEffects) -> bool {
    let mut changed = false;
    for &obj in &delta.reads {
        changed |= dst.reads.insert(obj);
    }
    for &obj in &delta.writes {
        changed |= dst.writes.insert(obj);
    }
    changed |= !dst.reads_unknown && delta.reads_unknown;
    dst.reads_unknown |= delta.reads_unknown;
    changed |= !dst.writes_unknown && delta.writes_unknown;
    dst.writes_unknown |= delta.writes_unknown;
    changed |= !dst.reads_io && delta.reads_io;
    dst.reads_io |= delta.reads_io;
    changed |= !dst.writes_io && delta.writes_io;
    dst.writes_io |= delta.writes_io;
    changed
}

impl std::ops::BitOrAssign for FunctionEffects {
    fn bitor_assign(&mut self, rhs: FunctionEffects) {
        self.reads.extend(rhs.reads);
        self.writes.extend(rhs.writes);
        self.reads_unknown |= rhs.reads_unknown;
        self.writes_unknown |= rhs.writes_unknown;
        self.reads_io |= rhs.reads_io;
        self.writes_io |= rhs.writes_io;
    }
}

/// Direct (call-free) effects of a function body.
fn direct_effects(program: &Program, func: Function) -> FunctionEffects {
    let mut fx = FunctionEffects::default();
    let data = program.func_data(func);
    let mut env = BaseEnv::new(data);
    let ctx = ArenaContext {
        program,
        curr_func: Some(func),
    };
    env.build_tables(&ctx, func);
    for bb_layout in data.layout().basicblocks() {
        for &inst in bb_layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Load(load) => {
                    add_address_effect(
                        &mut fx.reads,
                        &mut fx.reads_unknown,
                        &env,
                        &ctx,
                        func,
                        load.src(),
                    );
                }
                InstKind::Store(store) => {
                    add_address_effect(
                        &mut fx.writes,
                        &mut fx.writes_unknown,
                        &env,
                        &ctx,
                        func,
                        store.dest(),
                    );
                }
                InstKind::MemZero(mem_zero) => {
                    add_address_effect(
                        &mut fx.writes,
                        &mut fx.writes_unknown,
                        &env,
                        &ctx,
                        func,
                        mem_zero.dest(),
                    );
                }
                _ => {}
            }
        }
    }
    fx
}

fn add_address_effect(
    out: &mut HashSet<EffectObject>,
    out_unknown: &mut bool,
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    func: Function,
    addr: Inst,
) {
    match env.base_of(ctx, addr) {
        MemObject::Alloc(a) => {
            out.insert(EffectObject::Alloc(func, a));
        }
        MemObject::Global(g) => {
            out.insert(EffectObject::Global(g));
        }
        MemObject::Param(i) => {
            out.insert(EffectObject::Param(i));
        }
        MemObject::Unknown => *out_unknown = true,
    }
}

/// Classify a call-site actual argument into the concrete objects it may
/// point to.
fn classify_actual(
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    points_to: &HashMap<(Function, usize), HashSet<AbstractObject>>,
    func: Function,
    arg: Inst,
) -> HashSet<AbstractObject> {
    match env.base_of(ctx, arg) {
        MemObject::Alloc(a) => HashSet::from_iter([AbstractObject::Alloc(func, a)]),
        MemObject::Global(g) => HashSet::from_iter([AbstractObject::Global(g)]),
        MemObject::Param(i) => points_to.get(&(func, i)).cloned().unwrap_or_default(),
        MemObject::Unknown => HashSet::from_iter([AbstractObject::Unknown]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{
            Program, Type,
            builder_trait::{GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder},
        },
        opt::pass::{ArenaContext, ArenaContextMut},
    };

    fn new_global(program: &mut Program) -> Inst {
        let init = program.new_value().zero_init(Type::get_i32());
        program.new_value().global_alloc(init)
    }

    /// A function `name(params)` with an entry block (no terminator yet;
    /// callers append their own instructions and a `ret`).
    fn new_body(program: &mut Program, name: &str, params: Vec<Type>) -> Function {
        let function = program.new_function(Type::get_unit(), name.into(), params);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        data.add_entry_block();
        function
    }

    #[test]
    fn pure_function_without_memory_ops() {
        let mut program = Program::new();
        let f = new_body(&mut program, "f", vec![]);
        let analysis = EffectAnalysis::new(&program);
        assert!(analysis.is_pure(f));
        assert!(analysis.is_removable(f));
    }

    #[test]
    fn store_to_global_is_impure_and_recorded() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(f));
        assert!(!analysis.is_removable(f));
    }

    #[test]
    fn load_of_global_is_read_only_but_pure() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let load = data.new_local_value().load(global);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.reads.contains(&EffectObject::Global(global)));
        assert!(analysis.is_pure(f)); // reads are not observable
        assert!(analysis.is_removable(f));
    }

    #[test]
    fn callee_effects_propagate_to_caller() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(writer),
        };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let caller = new_body(&mut program, "caller", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(caller);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(caller));
    }

    #[test]
    fn param_write_is_substituted_at_call_site() {
        // `g` writes its parameter; `f` calls `g(global)` — f's write set
        // must contain the global.
        let mut program = Program::new();
        let global = new_global(&mut program);
        let g = program.new_function(
            Type::get_unit(),
            "g".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(g),
        };
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, param);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let f = new_body(&mut program, "f", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(g, vec![global]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(f));
    }

    #[test]
    fn getint_is_io_and_not_removable() {
        let mut program = Program::new();
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let f = new_body(&mut program, "f", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(getint);
        assert!(fx.reads_io);
        assert!(analysis.effects_of(f).reads_io); // propagated
        assert!(!analysis.is_removable(getint));
        assert!(!analysis.is_removable(f));
    }

    #[test]
    fn recursion_converges_conservatively() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, param);
        data.layout_mut().insert_inst(entry, store);
        let call = data.new_local_value().call(f, vec![param]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        // Recursive self-call converges; the param write is still visible.
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Param(0)));
    }

    #[test]
    fn points_to_refines_argument_vs_global_alias() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let global_b = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        // main passes global_a to f.
        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(f, vec![global_a]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let pts = analysis.points_to_of(f, 0).unwrap();
        assert!(pts.contains(&AbstractObject::Global(global_a)));

        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let param = program.func_data(f).params()[0];
        // param vs global_a: MayAlias (a may be passed to it)
        assert!(analysis.alias(&ctx, f, param, global_a).may_alias());
        // param vs global_b: provably NoAlias
        assert_eq!(
            analysis.alias(&ctx, f, param, global_b),
            AliasResult::NoAlias
        );
    }

    #[test]
    fn points_to_disjoint_params_do_not_alias() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let global_b = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        // Distinct globals: params can never alias.
        let call = data.new_local_value().call(f, vec![global_a, global_b]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let (p0, p1) = {
            let params = program.func_data(f).params();
            (params[0], params[1])
        };
        assert_eq!(analysis.alias(&ctx, f, p0, p1), AliasResult::NoAlias);
    }

    #[test]
    fn same_object_passed_twice_keeps_params_aliasing() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(f, vec![global_a, global_a]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let (p0, p1) = {
            let params = program.func_data(f).params();
            (params[0], params[1])
        };
        assert!(analysis.alias(&ctx, f, p0, p1).may_alias());
    }

    #[test]
    fn getarray_writes_its_argument() {
        let mut program = Program::new();
        let getarray = program.new_function(
            Type::get_i32(),
            "getarray".into(),
            vec![Type::get_i32().reference()],
        );
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_value()
            .alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let call = data.new_local_value().call(getarray, vec![alloc]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        // getarray writes into the local array: visible in f's write set.
        assert!(fx.writes.contains(&EffectObject::Alloc(f, alloc)));
        assert!(!analysis.is_pure(f));
        assert!(fx.reads_io);
    }

    #[test]
    fn local_allocs_never_alias_arguments() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(f),
        };
        let entry = data.add_entry_block();
        let alloc = data.new_local_value().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let param = program.func_data(f).params()[0];
        // Even with unknown points-to, a local never aliases an argument
        // of the same invocation (the frontend pointer-slot pattern makes
        // this the load-bearing case).
        assert_eq!(analysis.alias(&ctx, f, alloc, param), AliasResult::NoAlias);
    }
}
