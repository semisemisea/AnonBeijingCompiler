use std::{
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::{
    arena::{Arena, ArenaMut, LocalArena},
    builder::{BasicBlockBuilders, LocalBuilder},
    inst_kind::FuncArgRef,
    instruction::Inst,
    layout::Layout,
    types::Type,
};

pub struct FunctionData {
    ret_ty: Type,
    name: String,
    params: Vec<Inst>,
    layout: Layout,
    local_arena: LocalArena,
}

impl FunctionData {
    pub fn new(ret_ty: Type, name: String, params_ty: Vec<Type>) -> FunctionData {
        let mut local_arena = LocalArena::new();
        let params = params_ty
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                ArenaMut::new(Some(&mut local_arena), None)
                    .alloc_local_inst(FuncArgRef::new_data(i, ty.clone()))
            })
            .collect();
        FunctionData {
            ret_ty,
            name,
            params,
            layout: Layout::new(),
            local_arena,
        }
    }

    #[deprecated]
    pub fn dfg(&self) -> Arena<'_> {
        self.arena()
    }

    #[deprecated]
    pub fn dfg_mut(&mut self) -> ArenaMut<'_> {
        self.arena_mut()
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn layout_mut(&mut self) -> &mut Layout {
        &mut self.layout
    }

    pub fn arena(&self) -> Arena<'_> {
        Arena::new(Some(&self.local_arena), None)
    }

    pub fn arena_mut(&mut self) -> ArenaMut<'_> {
        ArenaMut::new(Some(&mut self.local_arena), None)
    }

    pub fn new_local_inst(&mut self) -> LocalBuilder<'_> {
        LocalBuilder {
            arena: ArenaMut {
                local: Some(&mut self.local_arena),
                global: None,
            },
        }
    }

    pub fn new_basic_block(&mut self) -> BasicBlockBuilders<'_> {
        BasicBlockBuilders {
            arena: ArenaMut {
                local: Some(&mut self.local_arena),
                global: None,
            },
        }
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ret_ty(&self) -> &Type {
        &self.ret_ty
    }

    pub fn params(&self) -> &[Inst] {
        &self.params
    }

    pub fn local_arena(&self) -> &LocalArena {
        &self.local_arena
    }

    pub fn local_arena_mut(&mut self) -> &mut LocalArena {
        &mut self.local_arena
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Function(NonZeroU32);
// pub type Function = NonZeroU32;

static FUNCTION_ID: AtomicU32 = AtomicU32::new(1);

pub(in crate::ir) fn next_function_id() -> Function {
    Function(unsafe { NonZeroU32::new_unchecked(FUNCTION_ID.fetch_add(1, Ordering::Relaxed)) })
}

pub struct FunctionArena {
    data: Vec<FunctionData>,
}

impl FunctionArena {
    pub fn new() -> FunctionArena {
        FunctionArena { data: Vec::new() }
    }

    pub fn data_of(&self, func: Function) -> &FunctionData {
        &self.data[(func.0.get() - 1) as usize]
    }

    pub fn mut_data_of(&mut self, func: Function) -> &mut FunctionData {
        &mut self.data[(func.0.get() - 1) as usize]
    }

    pub fn alloc(&mut self, func_data: FunctionData) {
        self.data.push(func_data);
    }

    pub fn functions(&self) -> impl Iterator<Item = (Function, &FunctionData)> {
        self.data.iter().enumerate().map(|(i, data)| {
            (
                unsafe { Function(NonZeroU32::new_unchecked(i as u32 + 1)) },
                data,
            )
        })
    }

    pub fn functions_mut(&mut self) -> impl Iterator<Item = (Function, &mut FunctionData)> {
        self.data.iter_mut().enumerate().map(|(i, data)| {
            (
                unsafe { Function(NonZeroU32::new_unchecked(i as u32 + 1)) },
                data,
            )
        })
    }
}
