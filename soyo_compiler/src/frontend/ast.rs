use super::items;
use crate::frontend::utils::{AstGenContext, Symbol, ToRaanaIR};
use raana_ir::ir::{arena::Arena, builder_trait::*, *};

impl ToRaanaIR for items::CompUnits {
    fn convert(&self, ctx: &mut AstGenContext) {
        ctx.decl_library_functions();
        for comp_unit in &self.comp_units {
            comp_unit.convert(ctx);
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::CompUnit {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::CompUnit::FuncDef(func_def) => func_def.convert(ctx),
            items::CompUnit::Decl(decl) => decl.global_convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::FuncDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        // Register the function to get handle
        let param_ty = self
            .params
            .iter()
            .map(|x| x.ty_global(ctx))
            .collect::<Vec<_>>();
        let func = ctx.program.new_function(
            self.func_type.clone(),
            self.ident.as_ref().to_string(),
            param_ty,
        );
        // Prologue
        // - Add function to the stack
        // - Insert the "entry" basic block and save it.
        // - Increse the scope depth.
        ctx.insert_func(self.ident.clone(), func);
        ctx.push_func(func);
        let entry_bb = ctx.add_entry_bb();
        ctx.set_curr_bb(entry_bb);
        // let prev_bb = ctx.set_curr_bb(entry_bb);

        // Recursive conversion.
        ctx.add_scope();
        let params = ctx.curr_func_data().params().to_vec();
        let ty_name_and_val = self.params.iter().cloned().zip(params.iter());
        for (param_slot, &param_val) in ty_name_and_val {
            let ty = param_slot.ty_global(ctx);
            let alloc = ctx.new_local_value().alloc(ty);
            ctx.set_value_name(alloc, param_slot.ident.clone().clone());
            let store = ctx.new_local_value().store(param_val, alloc);
            ctx.insert_var(param_slot.ident.clone(), alloc);
            ctx.push_inst(alloc);
            ctx.push_inst(store);
        }
        for block_item in &self.block.block_items {
            block_item.convert(ctx);
        }
        ctx.del_scope();

        // Epilogue at the end:
        // For all function we explicitly add return None at the end, equivalent to `return ;`
        // This is because non-void function must have return statement at the end, a.k.a completed
        // When a function is completed, any return statement added later is abandoned.
        // But void function can have implicit return statement, a.k.a incompleted
        // So we add extra return to fix it.
        let ret = ctx.new_local_value().ret(None);
        ctx.push_inst(ret);

        ctx.reset_curr_bb();
        ctx.pop_func();
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Block {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        ctx.add_scope();
        for block_item in &self.block_items {
            block_item.convert(ctx);
        }
        ctx.del_scope();
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::BlockItem {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::BlockItem::Decl(decl) => decl.convert(ctx),
            items::BlockItem::Stmt(stmt) => stmt.convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Decl {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::Decl::ConstDecl(c_decl) => c_decl.convert(ctx),
            items::Decl::VarDecl(v_decl) => v_decl.convert(ctx),
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::Decl::ConstDecl(c_decl) => c_decl.global_convert(ctx),
            items::Decl::VarDecl(v_decl) => v_decl.global_convert(ctx),
        }
    }
}

impl ToRaanaIR for items::ConstDecl {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        assert!(
            self.btype.is_i32(),
            "Unknown type for constant declaration."
        );
        ctx.set_def_type(self.btype.clone());
        for const_def in &self.const_defs {
            const_def.convert(ctx);
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        assert!(
            self.btype.is_i32(),
            "Unknown type for constant declaration."
        );
        ctx.set_def_type(self.btype.clone());
        for const_def in &self.const_defs {
            const_def.global_convert(ctx);
        }
    }
}

impl ToRaanaIR for items::ConstDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let ty = ctx.curr_def_type().unwrap();
        // not an array
        if self.arr_dim.is_empty() {
            // Get the init val
            let items::ConstInitVal::Normal(_) = self.const_init_val else {
                panic!("Invalid assign: array to a integer")
            };
            self.const_init_val.convert(ctx);
            let init_val = ctx.pop_val().unwrap();
            // Not a constant val
            // if !ctx.curr_func_data().dfg().value(init_val).kind().is_const() {
            //     panic!("Inst can't be calculated at compile time.");
            // };
            ctx.insert_const(self.ident.clone(), init_val)
        }
        // is an array
        else {
            let array_shape = self
                .arr_dim
                .iter()
                .rev()
                .map(|const_exp| {
                    const_exp.convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            let arr_ty = array_shape
                .iter()
                .rev()
                .map(|x| *x as usize)
                .rfold(ty, Type::get_array);
            let alloc_var = ctx.new_local_value().alloc(arr_ty);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);

            if !matches!(self.const_init_val, items::ConstInitVal::Array(_)) {
                panic!("Invalid assign: integer to an array")
            }
            let exps = self.const_init_val.init_val_shape(&array_shape);

            let zero = ctx.new_local_value().integer(0);
            let const_init_vals = exps
                .iter()
                .map(|const_exp| match const_exp {
                    Some(exp) => {
                        exp.convert(ctx);
                        ctx.pop_val().unwrap()
                    }
                    None => zero,
                })
                // TODO: We can change it to for loop to avoid `.collect()`
                .collect::<Vec<_>>();
            fn initializer(
                array_shape: &[i32],
                init_val: &mut impl Iterator<Item = Inst>,
                get_from: Inst,
                ctx: &mut AstGenContext,
            ) {
                if array_shape.is_empty() {
                    let store = ctx
                        .new_local_value()
                        .store(init_val.next().unwrap(), get_from);
                    ctx.push_inst(store);
                    return;
                }
                for offset in 0..*array_shape.last().unwrap() {
                    let index = ctx.new_local_value().integer(offset);
                    let get_elem_ptr = ctx.new_local_value().get_elem_ptr(get_from, index);
                    ctx.push_inst(get_elem_ptr);
                    initializer(
                        &array_shape[..array_shape.len() - 1],
                        init_val,
                        get_elem_ptr,
                        ctx,
                    );
                }
            }
            initializer(
                &array_shape,
                &mut const_init_vals.into_iter(),
                alloc_var,
                ctx,
            );
            ctx.insert_const(self.ident.clone(), alloc_var)
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        // is array
        if self.arr_dim.is_empty() {
            self.const_init_val.global_convert(ctx);
            let init_val = ctx.pop_val().unwrap();
            // No more check
            ctx.insert_const(self.ident.clone(), init_val)
        }
        // not an array
        else {
            let array_shape = self
                .arr_dim
                .iter()
                .map(|const_exp| {
                    const_exp.global_convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            if !matches!(self.const_init_val, items::ConstInitVal::Array(_)) {
                panic!("Invalid assign: integer to an array")
            };
            let exps = self.const_init_val.init_val_shape(&array_shape);
            let zero = ctx.new_global_value().integer(0);
            let elems = exps
                .iter()
                .map(|exp| match exp {
                    Some(exp) => {
                        exp.global_convert(ctx);
                        ctx.pop_val().unwrap()
                    }
                    None => zero,
                })
                .collect::<Vec<_>>();
            let agg = array_shape.iter().rev().fold(elems, |elems, &dim_l| {
                elems
                    .chunks(dim_l as _)
                    .map(|chunk| ctx.new_global_value().aggregate(chunk.to_owned()))
                    .collect::<Vec<_>>()
            });
            let init = *agg.first().unwrap();
            let alloc_var = ctx.new_global_value().global_alloc(init);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.insert_const(self.ident.clone(), alloc_var)
        }
    }
}

impl ToRaanaIR for items::ConstInitVal {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::ConstInitVal::Normal(const_exp) => const_exp.convert(ctx),
            items::ConstInitVal::Array(const_exps) => {
                for const_exp in const_exps {
                    const_exp.convert(ctx);
                }
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::ConstInitVal::Normal(const_exp) => const_exp.global_convert(ctx),
            items::ConstInitVal::Array(const_exps) => {
                for const_exp in const_exps {
                    const_exp.global_convert(ctx);
                }
            }
        }
    }
}

impl ToRaanaIR for items::ConstExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        self.exp.convert(ctx)
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        self.exp.global_convert(ctx)
    }
}

impl ToRaanaIR for items::VarDecl {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        assert!(self.btype.is_i32(), "Unknown type for variable declaration");
        ctx.set_def_type(self.btype.clone());
        for var_def in &self.var_defs {
            var_def.convert(ctx);
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        assert!(self.btype.is_i32(), "Unknown type for variable declaration");
        ctx.set_def_type(self.btype.clone());
        for var_def in &self.var_defs {
            var_def.global_convert(ctx);
        }
    }
}

impl ToRaanaIR for items::VarDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let ty = ctx.curr_def_type().unwrap();
        // Not an array
        if self.arr_dim.is_empty() {
            // Allocate a target type of variable.
            let alloc_var = ctx.new_local_value().alloc(ty);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);
            if let Some(ref init_val) = self.init_val {
                let items::InitVal::Normal(exp) = init_val else {
                    panic!("Invalid assign: array to a integer")
                };
                exp.convert(ctx);
                // store the calculated value.
                let val = ctx.pop_val().unwrap();
                let store = ctx.new_local_value().store(val, alloc_var);
                ctx.push_inst(store);
            }
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
        // is an array
        else {
            // for given expression like `a[x][y][z]`, we first take out each const exp in the []
            // bracket and calculated it as i32(only type we accept)
            // we calculate from `z` to `x`, and pop it from `x` to `z`.
            // then we could get the array shape [x, y, z] as Vec<i32>
            let array_shape = self
                .arr_dim
                .iter()
                .rev()
                .map(|const_exp| {
                    const_exp.convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            // But for array type, we built arr[z] first, then brr[y][z], finally crr[x][y][z]
            // so we need to do it in reverse order.
            // at the end we can allocate that type and give it a name.
            let arr_ty = array_shape
                .iter()
                .rev()
                .map(|x| *x as usize)
                .rfold(ty, Type::get_array);
            let alloc_var = ctx.new_local_value().alloc(arr_ty);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);

            // We handle the possible initial value.
            if let Some(ref init_val) = self.init_val {
                // must be an array
                if !matches!(init_val, items::InitVal::Array(_)) {
                    panic!("Invalid assign: integer to an array")
                };

                // Flatten it up, filling the missing init val with None
                // `a[2][2] = {{1}, 3}` => [Some(exp_1), None, Some(exp_3), None];
                // `a[2][2] = {1, 3}` => [Some(exp_1), Some(exp_3), None, None];
                let exps = init_val.init_val_shape(&array_shape);

                // Check every item, if `Some(exp)`, then calculate exp and take the value
                // if None, then fill it with default value zero
                let zero = ctx.new_local_value().integer(0);
                let init_vals = exps
                    .iter()
                    .map(|exp| match exp {
                        Some(exp) => {
                            exp.convert(ctx);
                            ctx.pop_val().unwrap()
                        }
                        None => zero,
                    })
                    .collect::<Vec<_>>();

                // Now store the initial value
                fn initializer(
                    array_shape: &[i32],
                    init_val: &mut impl Iterator<Item = Inst>,
                    get_from: Inst,
                    ctx: &mut AstGenContext,
                ) {
                    if array_shape.is_empty() {
                        let store = ctx
                            .new_local_value()
                            .store(init_val.next().unwrap(), get_from);
                        ctx.push_inst(store);
                        return;
                    }
                    for offset in 0..*array_shape.last().unwrap() {
                        let index = ctx.new_local_value().integer(offset);
                        let get_elem_ptr = ctx.new_local_value().get_elem_ptr(get_from, index);
                        ctx.push_inst(get_elem_ptr);
                        initializer(
                            &array_shape[..array_shape.len() - 1],
                            init_val,
                            get_elem_ptr,
                            ctx,
                        );
                    }
                }
                initializer(&array_shape, &mut init_vals.into_iter(), alloc_var, ctx);
            }
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        let ty = ctx.curr_def_type().unwrap();
        if self.arr_dim.is_empty() {
            let init_val = if let Some(ref init_val) = self.init_val {
                init_val.global_convert(ctx);
                ctx.pop_val().unwrap()
            } else {
                ctx.new_global_value().zero_init(ty.clone())
            };
            let val = ctx.new_global_value().global_alloc(init_val);
            ctx.set_value_name(val, self.ident.clone());
            ctx.insert_var(self.ident.clone(), val)
        } else {
            let array_shape = self
                .arr_dim
                .iter()
                .map(|const_exp| {
                    const_exp.global_convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            let arr_ty = array_shape
                .iter()
                .map(|x| *x as usize)
                .rfold(ty, Type::get_array);

            let init = if let Some(ref init_val) = self.init_val {
                if !matches!(init_val, items::InitVal::Array(_)) {
                    panic!("Invalid assign: integer to an array")
                }
                let exps = init_val.init_val_shape(&array_shape);
                let zero = ctx.new_global_value().integer(0);
                let elems = exps
                    .iter()
                    .map(|exp| match exp {
                        Some(exp) => {
                            exp.global_convert(ctx);
                            ctx.pop_val().unwrap()
                        }
                        None => zero,
                    })
                    .collect::<Vec<_>>();
                let agg = array_shape.iter().rev().fold(elems, |elems, &dim_l| {
                    elems
                        .chunks(dim_l as _)
                        .map(|chunk| ctx.new_global_value().aggregate(chunk.to_owned()))
                        .collect::<Vec<_>>()
                });
                agg[0]
            } else {
                ctx.new_global_value().zero_init(arr_ty)
            };
            let alloc_var = ctx.new_global_value().global_alloc(init);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
    }
}

impl ToRaanaIR for items::InitVal {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::InitVal::Normal(exp) => exp.convert(ctx),
            items::InitVal::Array(exps) => {
                for exp in exps {
                    exp.convert(ctx);
                }
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::InitVal::Normal(exp) => exp.global_convert(ctx),
            items::InitVal::Array(exps) => {
                for exp in exps {
                    exp.global_convert(ctx);
                }
            }
        }
    }
}

impl ToRaanaIR for items::Stmt {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::Stmt::Assign(assign_stmt) => assign_stmt.convert(ctx),
            items::Stmt::Return(return_stmt) => return_stmt.convert(ctx),
            items::Stmt::Block(block) => block.convert(ctx),
            items::Stmt::Single(exp) => {
                if let Some(exp) = exp {
                    exp.convert(ctx);
                }
            }
            items::Stmt::IfStmt(if_stmt) => if_stmt.convert(ctx),
            items::Stmt::WhileStmt(while_stmt) => while_stmt.convert(ctx),
            items::Stmt::Break(break_stmt) => break_stmt.convert(ctx),
            items::Stmt::Continue(continue_stmt) => continue_stmt.convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Break {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let loop_end = ctx
            .curr_loop()
            .unwrap_or_else(|| panic!("Use break outside of loop"))
            .1;
        let jump_to_loop_end = ctx.new_local_value().jump(loop_end, vec![]);
        ctx.push_inst(jump_to_loop_end);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Continue {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let loop_start = ctx
            .curr_loop()
            .unwrap_or_else(|| panic!("Use continue outside of loop"))
            .0;
        let jump_to_loop_start = ctx.new_local_value().jump(loop_start, vec![]);
        ctx.push_inst(jump_to_loop_start);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::WhileStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // create 3 basic blocks for while loop
        let entry = ctx
            .new_basic_block()
            .basic_block("while_entry".into(), vec![]);
        ctx.register_bb(entry);
        let body = ctx
            .new_basic_block()
            .basic_block("while_body".into(), vec![]);
        ctx.register_bb(body);
        let end = ctx
            .new_basic_block()
            .basic_block("while_end".into(), vec![]);
        ctx.register_bb(end);
        ctx.push_loop(entry, end);

        // jump into while entry block unconditionally
        let jump_to_while_entry = ctx.new_local_value().jump(entry, vec![]);
        ctx.push_inst(jump_to_while_entry);

        ctx.set_curr_bb(entry);
        self.cond.convert(ctx);
        let cond_val = ctx.pop_val().unwrap();
        let branch = ctx
            .new_local_value()
            .branch(cond_val, body, vec![], end, vec![]);
        ctx.push_inst(branch);

        ctx.set_curr_bb(body);
        self.body.convert(ctx);
        let jump = ctx.new_local_value().jump(entry, vec![]);
        ctx.push_inst(jump);

        ctx.pop_loop();
        ctx.set_curr_bb(end);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::ReturnStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let v_ret = match &self.exp {
            Some(ret_exp) => {
                ret_exp.convert(ctx);
                ctx.pop_val()
            }
            None => None,
        };
        let ret = ctx.new_local_value().ret(v_ret);
        ctx.push_inst(ret);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::IfStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // Get condition exp value.
        self.cond.convert(ctx);
        let cond_val = ctx.pop_val().unwrap();
        let then_bb = ctx.new_basic_block().basic_block("then".into(), vec![]);
        ctx.register_bb(then_bb);
        let else_bb = self.else_branch.as_ref().map(|_| {
            let bb = ctx.new_basic_block().basic_block("else".into(), vec![]);
            ctx.register_bb(bb);
            bb
        });
        let end_bb = ctx.new_basic_block().basic_block("end".into(), vec![]);
        ctx.register_bb(end_bb);
        let br = ctx.new_local_value().branch(
            cond_val,
            then_bb,
            vec![],
            else_bb.unwrap_or(end_bb),
            vec![],
        );
        ctx.push_inst(br);

        ctx.set_curr_bb(then_bb);
        self.then_branch.convert(ctx);
        let then_jump = ctx.new_local_value().jump(end_bb, vec![]);
        ctx.push_inst(then_jump);

        if let Some(else_bb) = else_bb {
            ctx.set_curr_bb(else_bb);
            self.else_branch.as_ref().unwrap().convert(ctx);
            let else_jump = ctx.new_local_value().jump(end_bb, vec![]);
            ctx.push_inst(else_jump);
        }

        ctx.set_curr_bb(end_bb);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::AssignStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        if ctx.is_constant(&self.l_val) {
            panic!("Can't modify a constant");
        }
        self.l_val.convert(ctx);
        let lhs_l_val = ctx.pop_val().unwrap();
        self.exp.convert(ctx);
        let rhs_exp = ctx.pop_val().unwrap();

        // Compile time type-check.
        let lhs_ptr_type = ctx.new_local_value().inst_type(lhs_l_val);
        let rhs_exp_type = ctx.new_local_value().inst_type(rhs_exp);
        assert!(
            Type::get_pointer(rhs_exp_type.clone()) == lhs_ptr_type.clone(),
            "Type not match. {rhs_exp_type} can't store in {lhs_ptr_type}"
        );
        let store = ctx.new_local_value().store(rhs_exp, lhs_l_val);
        ctx.push_inst(store);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Exp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        self.lor_exp.convert(ctx)
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        self.lor_exp.global_convert(ctx)
    }
}

impl ToRaanaIR for items::LOrExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::LOrExp::LAndExp(land_exp) => land_exp.convert(ctx),
            items::LOrExp::Comp(lor_exp, land_exp) => {
                // handle lhs
                lor_exp.convert(ctx);
                let lhs = ctx.pop_val().unwrap();

                // check if lhs != 0
                let zero = ctx.new_local_value().integer(0);
                let lhs_ne_0 = ctx.new_local_value().binary(BinaryOp::NotEq, lhs, zero);
                ctx.push_inst(lhs_ne_0);

                // two basic block for short circuit logic
                let rhs_bb = ctx.new_basic_block().basic_block("lor_rhs".into(), vec![]);
                ctx.register_bb(rhs_bb);
                let merge_bb = ctx
                    .new_basic_block()
                    .basic_block("lor_merge".into(), vec![Type::get_i32()]);
                ctx.register_bb(merge_bb);

                // short circuit logic
                let br = ctx.new_local_value().branch(
                    lhs_ne_0,
                    merge_bb,
                    vec![lhs_ne_0],
                    rhs_bb,
                    vec![],
                );
                ctx.push_inst(br);

                // check rhs
                let original = ctx.set_curr_bb(rhs_bb).unwrap();
                // ctx.set_curr_bb(rhs_bb).unwrap();
                land_exp.convert(ctx);
                let rhs = ctx.pop_val().unwrap();

                // Constant folding
                if let (InstKind::Integer(int_lhs), InstKind::Integer(int_rhs)) =
                    (ctx.inst_data(lhs).kind(), ctx.inst_data(rhs).kind())
                {
                    // Get lhs and rhs value.
                    let int_lhs = int_lhs.value();
                    let int_rhs = int_rhs.value();

                    // remove the previous instruction
                    ctx.set_curr_bb(original);
                    ctx.remove_inst(lhs_ne_0);
                    ctx.remove_inst(br);
                    ctx.remove_bb(rhs_bb);
                    ctx.remove_bb(merge_bb);

                    let result = ctx
                        .new_local_value()
                        .integer((int_lhs != 0 || int_rhs != 0) as _);
                    ctx.push_val(result);

                    return;
                }
                let rhs_ne_0 = ctx.new_local_value().binary(BinaryOp::NotEq, rhs, zero);
                ctx.push_inst(rhs_ne_0);

                // jump to the merge block and pass the information
                let jump = ctx.new_local_value().jump(merge_bb, vec![rhs_ne_0]);
                ctx.push_inst(jump);

                ctx.set_curr_bb(merge_bb);
                let result = ctx.bb_params(merge_bb)[0];
                ctx.push_val(result);
            }
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::LOrExp::LAndExp(land_exp) => land_exp.global_convert(ctx),
            items::LOrExp::Comp(lor_exp, land_exp) => {
                lor_exp.global_convert(ctx);
                let lhs_val = ctx.pop_val().unwrap();
                let lhs_int = ctx.get_global_val(lhs_val).unwrap();
                land_exp.global_convert(ctx);
                let rhs_val = ctx.pop_val().unwrap();
                let rhs_int = ctx.get_global_val(rhs_val).unwrap();
                let or_result = ctx
                    .program
                    .new_value()
                    .integer((lhs_int != 0 || rhs_int != 0) as i32);
                ctx.push_val(or_result);
            }
        }
    }
}

impl ToRaanaIR for items::LAndExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::LAndExp::EqExp(eq_exp) => eq_exp.convert(ctx),
            items::LAndExp::Comp(land_exp, eq_exp) => {
                // handle lhs
                land_exp.convert(ctx);
                let lhs = ctx.pop_val().unwrap();

                // check if lhs == 0
                let zero = ctx.new_local_value().integer(0);
                let lhs_eq_0 = ctx.new_local_value().binary(BinaryOp::Eq, lhs, zero);
                ctx.push_inst(lhs_eq_0);

                // two basic block for short circuit logic
                let rhs_bb = ctx.new_basic_block().basic_block("land_rhs".into(), vec![]);
                ctx.register_bb(rhs_bb);
                let merge_bb = ctx
                    .new_basic_block()
                    .basic_block("land_merge".into(), vec![Type::get_i32()]);
                ctx.register_bb(merge_bb);

                //short circuit logic
                let br =
                    ctx.new_local_value()
                        .branch(lhs_eq_0, merge_bb, vec![zero], rhs_bb, vec![]);
                ctx.push_inst(br);

                // check rhs
                let original = ctx.set_curr_bb(rhs_bb).unwrap();
                // ctx.set_curr_bb(rhs_bb).unwrap();
                eq_exp.convert(ctx);
                let rhs = ctx.pop_val().unwrap();
                // Constant folding
                if let (InstKind::Integer(int_lhs), InstKind::Integer(int_rhs)) =
                    (ctx.inst_data(lhs).kind(), ctx.inst_data(rhs).kind())
                {
                    // Get lhs and rhs value.
                    let int_lhs = int_lhs.value();
                    let int_rhs = int_rhs.value();

                    // remove the previous instruction
                    ctx.set_curr_bb(original);
                    ctx.remove_inst(lhs_eq_0);
                    ctx.remove_inst(br);
                    ctx.remove_bb(rhs_bb);
                    ctx.remove_bb(merge_bb);

                    let result = ctx
                        .new_local_value()
                        .integer((int_lhs != 0 && int_rhs != 0) as _);
                    ctx.push_val(result);

                    return;
                }

                let rhs_ne_0 = ctx.new_local_value().binary(BinaryOp::NotEq, rhs, zero);
                ctx.push_inst(rhs_ne_0);

                // jump to merge block and pass the information
                let jump = ctx.new_local_value().jump(merge_bb, vec![rhs_ne_0]);
                ctx.push_inst(jump);

                ctx.set_curr_bb(merge_bb);
                let result = ctx.bb_params(merge_bb)[0];
                ctx.push_val(result);
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::LAndExp::EqExp(eq_exp) => eq_exp.global_convert(ctx),
            items::LAndExp::Comp(land_exp, eq_exp) => {
                land_exp.global_convert(ctx);
                let lhs_val = ctx.pop_val().unwrap();
                let lhs_int = ctx.get_global_val(lhs_val).unwrap();
                eq_exp.global_convert(ctx);
                let rhs_val = ctx.pop_val().unwrap();
                let rhs_int = ctx.get_global_val(rhs_val).unwrap();
                let and_result = ctx
                    .program
                    .new_value()
                    .integer((lhs_int != 0 && rhs_int != 0) as i32);
                ctx.push_val(and_result);
            }
        }
    }
}

impl ToRaanaIR for items::EqExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::EqExp::RelExp(rel_exp) => rel_exp.convert(ctx),
            items::EqExp::Comp(lhs_eq, op, rhs_rel) => {
                lhs_eq.convert(ctx);
                rhs_rel.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::EqExp::RelExp(rel_exp) => rel_exp.global_convert(ctx),
            items::EqExp::Comp(eq_exp, binary_op, rel_exp) => {
                eq_exp.global_convert(ctx);
                rel_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::RelExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::RelExp::AddExp(add_exp) => add_exp.convert(ctx),
            items::RelExp::Comp(lhs_rel, op, rhs_add) => {
                lhs_rel.convert(ctx);
                rhs_add.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::RelExp::AddExp(add_exp) => add_exp.global_convert(ctx),
            items::RelExp::Comp(rel_exp, binary_op, add_exp) => {
                rel_exp.global_convert(ctx);
                add_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::AddExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::AddExp::MulExp(mul_exp) => mul_exp.convert(ctx),
            items::AddExp::Comp(lhs_add, op, rhs_mul) => {
                lhs_add.convert(ctx);
                rhs_mul.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::AddExp::MulExp(mul_exp) => mul_exp.global_convert(ctx),
            items::AddExp::Comp(add_exp, binary_op, mul_exp) => {
                add_exp.global_convert(ctx);
                mul_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::MulExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::MulExp::UnaryExp(unary_exp) => unary_exp.convert(ctx),
            items::MulExp::Comp(lhs_mul, op, rhs_unary) => {
                lhs_mul.convert(ctx);
                rhs_unary.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::MulExp::UnaryExp(unary_exp) => unary_exp.global_convert(ctx),
            items::MulExp::Comp(mul_exp, binary_op, unary_exp) => {
                mul_exp.global_convert(ctx);
                unary_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::UnaryExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::UnaryExp::PrimaryExp(exp) => exp.convert(ctx),
            items::UnaryExp::Unary(unary_op, unary_exp) => {
                unary_exp.convert(ctx);
                unary_op.convert(ctx)
            }
            items::UnaryExp::FuncCall(func_call) => func_call.convert(ctx),
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::UnaryExp::PrimaryExp(primary_exp) => primary_exp.global_convert(ctx),
            items::UnaryExp::Unary(unary_op, unary_exp) => {
                unary_exp.global_convert(ctx);
                unary_op.global_convert(ctx)
            }
            items::UnaryExp::FuncCall(_) => panic!("Const function is not supported"),
        }
    }
}

impl ToRaanaIR for items::FuncCall {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let args = self
            .args
            .iter()
            .map(|exp| {
                exp.convert(ctx);
                let arg = ctx.pop_val().unwrap();
                if ctx.is_pointer_to_array(arg) {
                    let zero = ctx.new_local_value().integer(0);
                    let get_elem_ptr = ctx.new_local_value().get_elem_ptr(arg, zero);
                    ctx.push_inst(get_elem_ptr);
                    get_elem_ptr
                } else {
                    arg
                }
            })
            .collect::<Vec<_>>();
        let Symbol::Callable(target_func) = ctx
            .get_global(&self.ident)
            .unwrap_or_else(|| panic!("Can't find function {}", &*self.ident))
        else {
            panic!("Not a function {}", &*self.ident)
        };
        let call = ctx.new_local_value().call(target_func, args);
        ctx.push_inst(call);
        if !ctx.inst_data(call).ty().is_unit() {
            ctx.push_val(call);
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::PrimaryExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::PrimaryExp::Exp(exp) => exp.convert(ctx),
            items::PrimaryExp::Number(num) => {
                let num = ctx.new_local_value().integer(*num);
                ctx.push_val(num);
            }
            // LVal on the right side.
            // Meaning it's not defining but using a variable.
            // We take the value and push to value stack to use.
            items::PrimaryExp::LVal(l_val) => {
                // not a array
                if l_val.index.is_empty() {
                    match ctx.get_symbol(&l_val.ident).unwrap() {
                        Symbol::Constant(const_val) => {
                            let int = ctx.as_i32_val(const_val);
                            ctx.push_val(int);
                        }
                        Symbol::Variable(var_ptr) => {
                            if ctx.is_pointer_to_array(var_ptr) {
                                ctx.push_val(var_ptr);
                            } else {
                                let load = ctx.new_local_value().load(var_ptr);
                                ctx.push_inst(load);
                                ctx.push_val(load);
                            }
                        }
                        Symbol::Callable(..) => {
                            panic!("You might forget to call the function.")
                        }
                    }
                }
                // visiting an array
                else {
                    let offset = l_val
                        .index
                        .iter()
                        .map(|x| {
                            x.convert(ctx);
                            ctx.pop_val().unwrap()
                        })
                        .collect::<Vec<_>>();
                    match ctx.get_symbol(&l_val.ident).unwrap() {
                        Symbol::Constant(array) | Symbol::Variable(array) => {
                            let get_from = offset.iter().fold(array, |get_from, &index| {
                                let inst = if ctx.is_pointer_to_array(get_from) {
                                    ctx.new_local_value().get_elem_ptr(get_from, index)
                                } else {
                                    let load = ctx.new_local_value().load(get_from);
                                    ctx.push_inst(load);
                                    ctx.new_local_value().get_ptr(load, index)
                                };
                                ctx.push_inst(inst);
                                inst
                            });
                            if ctx.is_pointer_to_array(get_from) {
                                ctx.push_val(get_from);
                            } else {
                                let load = ctx.new_local_value().load(get_from);
                                ctx.push_inst(load);
                                ctx.push_val(load);
                            }
                        }
                        Symbol::Callable(_function) => panic!("Function can not be indexed."),
                    }
                }
            }
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::PrimaryExp::Exp(exp) => exp.global_convert(ctx),
            items::PrimaryExp::LVal(lval) => {
                let sym = ctx
                    .global_scope()
                    .get(&lval.ident)
                    .unwrap_or_else(|| panic!("{} not defined", &*lval.ident));
                let int = match sym {
                    Symbol::Constant(int) => {
                        let borrow_value = ctx.inst_data(*int);
                        let InstKind::Integer(int) = borrow_value.kind() else {
                            unreachable!();
                        };
                        int.value()
                    }
                    Symbol::Variable(var) => {
                        let borrow_value = ctx.inst_data(*var);
                        let InstKind::GlobalAlloc(glob_alloc) = borrow_value.kind() else {
                            unreachable!();
                        };
                        match ctx.inst_data(glob_alloc.init()).kind() {
                            InstKind::Integer(int) => int.value(),
                            InstKind::ZeroInit => 0,
                            _ => unreachable!(),
                        }
                    }
                    Symbol::Callable(_) => unreachable!(),
                };
                let val = ctx.new_global_value().integer(int);
                ctx.push_val(val);
            }
            items::PrimaryExp::Number(num) => {
                let num_lit = ctx.new_global_value().integer(*num);
                ctx.push_val(num_lit);
            }
        }
    }
}

impl ToRaanaIR for items::LVal {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let symbol = ctx
            .get_symbol(&self.ident)
            .unwrap_or_else(|| panic!("Variable {} not exists.", &*self.ident));
        let val = match symbol {
            Symbol::Constant(const_val) => panic!("Cannot modify a constant {const_val:?}"),
            Symbol::Variable(p_val) => {
                if self.index.is_empty() {
                    p_val
                } else {
                    let indices = self
                        .index
                        .iter()
                        .map(|exp| {
                            exp.convert(ctx);
                            ctx.pop_val().unwrap()
                        })
                        .collect::<Vec<_>>();
                    indices.iter().fold(p_val, |get_from, &offset| {
                        let p = if ctx.is_pointer_to_array(get_from) {
                            ctx.new_local_value().get_elem_ptr(get_from, offset)
                        } else {
                            let n_get_from = ctx.new_local_value().load(get_from);
                            ctx.push_inst(n_get_from);
                            ctx.new_local_value().get_ptr(n_get_from, offset)
                        };
                        ctx.push_inst(p);
                        p
                    })
                }
            }
            Symbol::Callable(func_handle) => {
                panic!("Cannot assign a value to a function {func_handle:?}")
            }
        };
        ctx.push_val(val);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        panic!("No corresponding syntax")
    }
}

impl ToRaanaIR for BinaryOp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let rhs = ctx.pop_val().unwrap();
        let lhs = ctx.pop_val().unwrap();

        // Constant folding
        if let (Some(int_lhs), Some(int_rhs)) = (ctx.as_i32(lhs), ctx.as_i32(rhs)) {
            let res = match self {
                BinaryOp::NotEq => (int_lhs != int_rhs) as i32,
                BinaryOp::Eq => (int_lhs == int_rhs) as i32,
                BinaryOp::Gt => (int_lhs > int_rhs) as i32,
                BinaryOp::Lt => (int_lhs < int_rhs) as i32,
                BinaryOp::Ge => (int_lhs >= int_rhs) as i32,
                BinaryOp::Le => (int_lhs <= int_rhs) as i32,
                BinaryOp::Add => int_lhs.wrapping_add(int_rhs),
                BinaryOp::Sub => int_lhs.wrapping_sub(int_rhs),
                BinaryOp::Mul => int_lhs.wrapping_mul(int_rhs),
                BinaryOp::Div => {
                    if int_rhs == 0 {
                        panic!("Division by zero");
                    } else {
                        int_lhs.wrapping_div(int_rhs)
                    }
                }
                BinaryOp::Rem => {
                    if int_rhs == 0 {
                        panic!("Modulo by zero");
                    } else {
                        int_lhs.wrapping_rem(int_rhs)
                    }
                }
                BinaryOp::And => int_lhs & int_rhs,
                BinaryOp::Or => int_lhs | int_rhs,
                BinaryOp::Xor => int_lhs ^ int_rhs,
                BinaryOp::Shl => int_lhs.wrapping_shl(int_rhs as u32),
                BinaryOp::Shr => (int_lhs as u32).wrapping_shr(int_rhs as u32) as i32,
                BinaryOp::Sar => int_lhs.wrapping_shr(int_rhs as u32),
            };

            let val = ctx.new_local_value().integer(res);
            ctx.push_val(val);
            return;
        }

        let operation = ctx.new_local_value().binary(*self, lhs, rhs);
        ctx.push_val(operation);
        ctx.push_inst(operation);
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        let rhs = ctx.pop_val().unwrap();
        let lhs = ctx.pop_val().unwrap();
        let res = if let (InstKind::Integer(lhs), InstKind::Integer(rhs)) =
            (ctx.inst_data(lhs).kind(), ctx.inst_data(rhs).kind())
        {
            let int_lhs = lhs.value();
            let int_rhs = rhs.value();
            match self {
                BinaryOp::NotEq => (int_lhs != int_rhs) as i32,
                BinaryOp::Eq => (int_lhs == int_rhs) as i32,
                BinaryOp::Gt => (int_lhs > int_rhs) as i32,
                BinaryOp::Lt => (int_lhs < int_rhs) as i32,
                BinaryOp::Ge => (int_lhs >= int_rhs) as i32,
                BinaryOp::Le => (int_lhs <= int_rhs) as i32,
                BinaryOp::Add => int_lhs.wrapping_add(int_rhs),
                BinaryOp::Sub => int_lhs.wrapping_sub(int_rhs),
                BinaryOp::Mul => int_lhs.wrapping_mul(int_rhs),
                BinaryOp::Div => {
                    if int_rhs == 0 {
                        panic!("Division by zero");
                    } else {
                        int_lhs.wrapping_div(int_rhs)
                    }
                }
                BinaryOp::Rem => {
                    if int_rhs == 0 {
                        panic!("Modulo by zero");
                    } else {
                        int_lhs.wrapping_rem(int_rhs)
                    }
                }
                BinaryOp::And => int_lhs & int_rhs,
                BinaryOp::Or => int_lhs | int_rhs,
                BinaryOp::Xor => int_lhs ^ int_rhs,
                BinaryOp::Shl => int_lhs.wrapping_shl(int_rhs as u32),
                BinaryOp::Shr => (int_lhs as u32).wrapping_shr(int_rhs as u32) as i32,
                BinaryOp::Sar => int_lhs.wrapping_shr(int_rhs as u32),
            }
        } else {
            unreachable!()
        };
        let val = ctx.new_global_value().integer(res);
        ctx.push_val(val);
    }
}

impl ToRaanaIR for items::UnaryOp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // if `+` is unary then it will do nothing.
        if matches!(self, items::UnaryOp::Add) {
            return;
        }

        let rhs = ctx.pop_val().unwrap();

        //Constant folding
        let rhs_val = ctx.inst_data(rhs);
        if let InstKind::Integer(integer) = rhs_val.kind().clone() {
            let operation = match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => ctx.new_local_value().integer(-integer.value()),
                items::UnaryOp::Negation => {
                    ctx.new_local_value().integer((integer.value() == 0) as _)
                }
            };
            ctx.push_val(operation);
            return;
        }

        let zero = ctx.new_local_value().integer(0);
        let operation = match self {
            items::UnaryOp::Add => unreachable!(),
            items::UnaryOp::Minus => ctx.new_local_value().binary(BinaryOp::Sub, zero, rhs),
            items::UnaryOp::Negation => ctx.new_local_value().binary(BinaryOp::Eq, zero, rhs),
        };
        ctx.push_val(operation);
        ctx.push_inst(operation);
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        if matches!(self, items::UnaryOp::Add) {
            return;
        }
        let rhs = ctx.pop_val().unwrap();
        let res = if let InstKind::Integer(int) = ctx.inst_data(rhs).kind() {
            match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => -int.value(),
                items::UnaryOp::Negation => (int.value() == 0) as i32,
            }
        } else {
            unreachable!()
        };
        let val = ctx.new_global_value().integer(res);
        ctx.push_val(val);
    }
}
