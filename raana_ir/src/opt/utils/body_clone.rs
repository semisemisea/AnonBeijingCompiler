use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{
        BasicBlock, Function, Inst, InstData, InstKind, Program, Type,
        arena::Arena,
        builder_trait::{BasicBlockBuilder, ScalarInstBuilder},
        remap::EntityMapper,
    },
    opt::pass::ArenaContextMut,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloneError {
    Declaration,
    MissingEntry,
    MissingLocalOperand(Inst),
    MissingTargetBlock(BasicBlock),
    InvalidLayoutParent(Inst),
    EmptyBlock(BasicBlock),
    InvalidTerminator(BasicBlock),
    LocalGlobalAlloc(Inst),
}

struct BlockClonePlan {
    source: BasicBlock,
    name: String,
    params: Vec<Inst>,
    insts: Vec<Inst>,
}

pub(crate) struct BodyClonePlan {
    source_name: String,
    entry: BasicBlock,
    blocks: Vec<BlockClonePlan>,
    values: Vec<(Inst, InstData)>,
    contains_tail_call: bool,
}

pub(crate) struct ClonedBody {
    pub(crate) entry: BasicBlock,
    pub(crate) blocks: Vec<BasicBlock>,
    pub(crate) returns: Vec<Inst>,
    pub(crate) tail_calls: Vec<Inst>,
}

impl BodyClonePlan {
    pub(crate) fn capture(program: &Program, source: Function) -> Result<Self, CloneError> {
        let data = program.func_data(source);
        if data.layout().is_decl() {
            return Err(CloneError::Declaration);
        }
        let entry = data
            .layout()
            .entry_bb()
            .map(|layout| layout.bb())
            .ok_or(CloneError::MissingEntry)?;

        let layout_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<FxHashSet<_>>();
        let mut blocks = Vec::with_capacity(layout_blocks.len());
        let mut roots = Vec::new();
        let mut contains_tail_call = false;

        for layout in data.layout().basicblocks() {
            let block = layout.bb();
            let insts = layout.insts().iter().copied().collect::<Vec<_>>();
            if insts.is_empty() {
                return Err(CloneError::EmptyBlock(block));
            }
            for (index, &inst) in insts.iter().enumerate() {
                if data.layout().parent_bb(inst) != Some(block) {
                    return Err(CloneError::InvalidLayoutParent(inst));
                }
                let is_terminator = data.inst_data(inst).kind().is_terminator();
                if is_terminator != (index + 1 == insts.len()) {
                    return Err(CloneError::InvalidTerminator(block));
                }
                if matches!(data.inst_data(inst).kind(), InstKind::TailCall(..)) {
                    contains_tail_call = true;
                }
                for target in data.inst_data(inst).bb_usage() {
                    if !layout_blocks.contains(&target) {
                        return Err(CloneError::MissingTargetBlock(target));
                    }
                }
            }

            let params = data.bb_data(block).params().to_vec();
            roots.extend(params.iter().copied());
            roots.extend(insts.iter().copied());
            blocks.push(BlockClonePlan {
                source: block,
                name: data.bb_data(block).name().to_owned(),
                params,
                insts,
            });
        }

        let mut values = Vec::new();
        let mut visited = FxHashSet::default();
        let mut queue = std::collections::VecDeque::from(roots);
        while let Some(inst) = queue.pop_front() {
            if inst.is_global() || !visited.insert(inst) {
                continue;
            }
            if !data.has_inst_data(inst) {
                return Err(CloneError::MissingLocalOperand(inst));
            }
            let inst_data = data.inst_data(inst);
            if matches!(inst_data.kind(), InstKind::GlobalAlloc(..)) {
                return Err(CloneError::LocalGlobalAlloc(inst));
            }
            queue.extend(
                inst_data
                    .inst_usage()
                    .filter(|operand| !operand.is_global()),
            );
            values.push((inst, inst_data.clone()));
        }

        Ok(Self {
            source_name: data.name().to_owned(),
            entry,
            blocks,
            values,
            contains_tail_call,
        })
    }

    pub(crate) fn contains_tail_call(&self) -> bool {
        self.contains_tail_call
    }

    pub(crate) fn clone_into(
        self,
        program: &mut Program,
        destination: Function,
        insert_after: BasicBlock,
    ) -> Result<ClonedBody, CloneError> {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(destination),
        };
        let value_data = self
            .values
            .iter()
            .map(|(inst, data)| (*inst, data))
            .collect::<FxHashMap<_, _>>();
        let mut block_map = FxHashMap::default();
        let mut value_map = FxHashMap::default();
        let mut cloned_blocks = Vec::with_capacity(self.blocks.len());
        let mut anchor = insert_after;

        for block in &self.blocks {
            let param_types = block
                .params
                .iter()
                .map(|param| value_data[param].ty().clone())
                .collect::<Vec<Type>>();
            let cloned = context.new_basic_block().basic_block(
                format!("{}_{}_inline", block.name, self.source_name),
                param_types,
            );
            context.layout_mut().insert_bb_after(anchor, cloned);
            anchor = cloned;
            block_map.insert(block.source, cloned);
            cloned_blocks.push(cloned);

            let cloned_params = context.bb_data(cloned).params().to_vec();
            for (&source_param, &cloned_param) in block.params.iter().zip(&cloned_params) {
                value_map.insert(source_param, cloned_param);
                if let Some(name) = value_data[&source_param].name() {
                    context.inst_data_mut(cloned_param).set_name(name.clone());
                }
            }
        }

        for &(source_inst, ref data) in &self.values {
            if value_map.contains_key(&source_inst) {
                continue;
            }
            let shell = context.new_local_value().undef(data.ty().clone());
            value_map.insert(source_inst, shell);
        }

        let mut mapper = CloneMapper {
            values: &value_map,
            blocks: &block_map,
        };
        for &(source_inst, ref data) in &self.values {
            if matches!(data.kind(), InstKind::BlockArgRef(..)) {
                continue;
            }
            let mapped = data.remap_refs(&mut mapper)?;
            context
                .replace_inst_with(value_map[&source_inst])
                .raw(mapped);
        }

        let mut returns = Vec::new();
        let mut tail_calls = Vec::new();
        for block in &self.blocks {
            let cloned_block = block_map[&block.source];
            for &source_inst in &block.insts {
                let cloned_inst = value_map[&source_inst];
                context.layout_mut().insert_inst(cloned_block, cloned_inst);
                match value_data[&source_inst].kind() {
                    InstKind::Return(..) => returns.push(cloned_inst),
                    InstKind::TailCall(..) => tail_calls.push(cloned_inst),
                    _ => {}
                }
            }
        }

        Ok(ClonedBody {
            entry: block_map[&self.entry],
            blocks: cloned_blocks,
            returns,
            tail_calls,
        })
    }
}

struct CloneMapper<'a> {
    values: &'a FxHashMap<Inst, Inst>,
    blocks: &'a FxHashMap<BasicBlock, BasicBlock>,
}

impl EntityMapper for CloneMapper<'_> {
    type Error = CloneError;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        if inst.is_global() {
            Ok(inst)
        } else {
            self.values
                .get(&inst)
                .copied()
                .ok_or(CloneError::MissingLocalOperand(inst))
        }
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        self.blocks
            .get(&block)
            .copied()
            .ok_or(CloneError::MissingTargetBlock(block))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ir::{BinaryOp, InstKind, Program, Type, arena::Arena, builder_trait::*},
        opt::utils::body_clone::BodyClonePlan,
    };

    #[test]
    fn clones_blocks_parameters_and_orphan_values_into_another_function() {
        let mut program = Program::new();
        let source = program.new_function(Type::get_i32(), "source".into(), vec![Type::get_i32()]);
        let source_entry = {
            let data = program.func_data_mut(source);
            let entry = data.add_entry_block();
            let param = data.params()[0];
            let one = data.new_local_inst().integer(1);
            let add = data.new_local_inst().binary(BinaryOp::Add, param, one);
            let ret = data.new_local_inst().ret(Some(add));
            data.layout_mut().insert_inst(entry, add);
            data.layout_mut().insert_inst(entry, ret);
            entry
        };
        let destination = program.new_function(Type::get_i32(), "destination".into(), vec![]);
        let destination_entry = {
            let data = program.func_data_mut(destination);
            let entry = data.add_entry_block();
            let zero = data.new_local_inst().integer(0);
            let ret = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(entry, ret);
            entry
        };

        let plan = BodyClonePlan::capture(&program, source).unwrap();
        let cloned = plan
            .clone_into(&mut program, destination, destination_entry)
            .unwrap();

        assert_ne!(cloned.entry, source_entry);
        assert_eq!(cloned.blocks.len(), 1);
        assert_eq!(cloned.returns.len(), 1);
        assert!(cloned.tail_calls.is_empty());
        let data = program.func_data(destination);
        let cloned_params = data.bb_data(cloned.entry).params();
        assert_eq!(cloned_params.len(), 1);
        let cloned_insts = data
            .layout()
            .basicblock(cloned.entry)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(cloned_insts.len(), 2);
        let add = cloned_insts[0];
        let InstKind::Binary(binary) = data.inst_data(add).kind() else {
            panic!("expected cloned binary");
        };
        assert_eq!(binary.lhs(), cloned_params[0]);
        assert!(data.has_inst_data(binary.rhs()));
        assert_eq!(data.layout().parent_bb(binary.rhs()), None);
    }
}
