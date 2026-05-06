use crate::opt::prelude::*;

pub struct StrengthReduction;

impl Pass for StrengthReduction {
    fn run_on(&self, data: &mut ArenaContext<'_>) {
        self.local_reduction(data);
    }
}

impl StrengthReduction {
    fn local_reduction(&self, data: &mut ArenaContext<'_>) {
        let to_change = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&val| matches!(data.inst_data(val).kind(), InstKind::Binary(_)))
            .copied()
            .collect::<Vec<_>>();

        for val in to_change {
            let InstKind::Binary(binary) = data.inst_data(val).kind() else {
                unreachable!()
            };
            match binary.op() {
                BinaryOp::Mul => {
                    if let InstKind::Integer(int) = data.inst_data(binary.lhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset =
                                data.new_local_inst().integer(po2.trailing_zeros() as i32);
                            data.replace_inst_with(val)
                                .binary(BinaryOp::Shl, shl_base, shl_offset);
                        }
                    } else if let InstKind::Integer(int) = data.inst_data(binary.rhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset =
                                data.new_local_inst().integer(po2.trailing_zeros() as i32);
                            data.replace_inst_with(val)
                                .binary(BinaryOp::Shl, shl_base, shl_offset);
                        }
                    }
                }
                BinaryOp::Div => {
                    if let InstKind::Integer(int) = data.inst_data(binary.rhs()).kind() {
                        if int.value().is_positive() && (int.value() as u32).is_power_of_two() {
                            let po2 = int.value() as u32;
                            let shl_base = binary.lhs();
                            let shl_offset =
                                data.new_local_inst().integer(po2.trailing_zeros() as i32);
                            data.replace_inst_with(val)
                                .binary(BinaryOp::Sar, shl_base, shl_offset);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
