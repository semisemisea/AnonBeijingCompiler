use crate::prelude::*;

impl ArenaContext<'_> {
    /// INFO: Call is consider to having side effect because do DCE in HLIR already.
    /// In HLIR's DCE, pure function with unused return value can be treated
    /// as having no side effect.
    pub fn has_side_effect_when_lowering(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_call() || inst.is_load() || inst.is_store()
    }

    pub fn is_terminator(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_terminator()
    }

    pub fn is_branch(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_branch()
    }
}
