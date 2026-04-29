use crate::{
    ir::{BinaryOp, Function, FunctionData, InstKind, builder_trait::LocalInstBuilder},
    opt::pass::Pass,
};

pub struct StrengthReduction;

impl Pass for StrengthReduction {
    fn run_on(&mut self, func: Function, data: &mut FunctionData) {
        self.local_reduction(data);
    }
}

impl StrengthReduction {
    fn local_reduction(&self, data: &mut FunctionData) {
        let to_change = data
            .layout()
            .bbs()
            .iter()
            .flat_map(|(_, node)| node.insts().keys())
            .filter(|&&val| matches!(data.dfg().value(val).kind(), InstKind::Binary(_)))
            .copied()
            .collect::<Vec<_>>();

        for val in to_change {
            let InstKind::Binary(binary) = data.dfg().value(val).kind() else {
                unreachable!()
            };
            match binary.op() {
                BinaryOp::Mul => {
                    if let InstKind::Integer(int) = data.dfg().value(binary.lhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset = data
                                .dfg_mut()
                                .new_value()
                                .integer(po2.trailing_zeros() as i32);
                            data.dfg_mut().replace_value_with(val).binary(
                                BinaryOp::Shl,
                                shl_base,
                                shl_offset,
                            );
                        }
                    } else if let InstKind::Integer(int) = data.dfg().value(binary.rhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset = data
                                .dfg_mut()
                                .new_value()
                                .integer(po2.trailing_zeros() as i32);
                            data.dfg_mut().replace_value_with(val).binary(
                                BinaryOp::Shl,
                                shl_base,
                                shl_offset,
                            );
                        }
                    }
                }
                BinaryOp::Div => {
                    if let InstKind::Integer(int) = data.dfg().value(binary.rhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset = data
                                .dfg_mut()
                                .new_value()
                                .integer(po2.trailing_zeros() as i32);
                            data.dfg_mut().replace_value_with(val).binary(
                                BinaryOp::Sar,
                                shl_base,
                                shl_offset,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
