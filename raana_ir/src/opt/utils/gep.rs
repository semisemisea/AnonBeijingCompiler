//! Type-driven GEP stride queries shared by loop and target optimizations.

use crate::ir::types::POINTER_SIZE;
use crate::ir::{Inst, InstKind, Type, TypeKind, arena::Arena};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GepIndexStride {
    pub byte_stride: i64,
    pub result_element_stride: i32,
}

pub fn gep_index_stride<A: Arena + ?Sized>(
    data: &A,
    gep_inst: Inst,
    index_position: usize,
) -> Option<GepIndexStride> {
    let InstKind::GetElemPtr(gep) = data.inst_data(gep_inst).kind() else {
        return None;
    };
    if index_position >= gep.offsets().len() {
        return None;
    }

    let mut current_ty = data.inst_data(gep.base()).ty().clone();
    let mut index_byte_stride = None;
    for position in 0..gep.offsets().len() {
        current_ty = match current_ty.kind() {
            TypeKind::Pointer(element) | TypeKind::Array(element, _) => element.clone(),
            _ => return None,
        };
        if position == index_position {
            index_byte_stride = Some(checked_type_size(&current_ty)?);
        }
    }

    let result_ty = data.inst_data(gep_inst).ty();
    let TypeKind::Pointer(result_element_ty) = result_ty.kind() else {
        return None;
    };
    debug_assert_eq!(&current_ty.reference(), result_ty);

    let byte_stride = index_byte_stride?;
    let result_element_size = checked_type_size(result_element_ty)?;
    if result_element_size == 0 || byte_stride % result_element_size != 0 {
        return None;
    }
    Some(GepIndexStride {
        byte_stride: i64::try_from(byte_stride).ok()?,
        result_element_stride: i32::try_from(byte_stride / result_element_size).ok()?,
    })
}

pub fn gep_constant_offset_with_replacement_fits<A: Arena + ?Sized>(
    data: &A,
    base: Inst,
    offsets: &[Inst],
    replacement_position: usize,
    replacement: i32,
) -> bool {
    let mut current_ty = data.inst_data(base).ty().clone();
    let mut constant_offset = 0_i64;
    for (position, &index) in offsets.iter().enumerate() {
        current_ty = match current_ty.kind() {
            TypeKind::Pointer(element) | TypeKind::Array(element, _) => element.clone(),
            _ => return false,
        };
        let Some(stride) = checked_type_size(&current_ty).and_then(|size| i64::try_from(size).ok())
        else {
            return false;
        };
        let constant = if position == replacement_position {
            Some(replacement)
        } else {
            match data.inst_data(index).kind() {
                InstKind::Integer(integer) => Some(integer.value()),
                _ => None,
            }
        };
        if let Some(constant) = constant {
            let Some(contribution) = i64::from(constant).checked_mul(stride) else {
                return false;
            };
            let Some(offset) = constant_offset.checked_add(contribution) else {
                return false;
            };
            constant_offset = offset;
        }
    }
    true
}

fn checked_type_size(ty: &Type) -> Option<usize> {
    match ty.kind() {
        TypeKind::ArgList | TypeKind::Unit => Some(0),
        TypeKind::Int32 | TypeKind::Float32 => Some(4),
        TypeKind::Array(element, len) => checked_type_size(element)?.checked_mul(*len),
        TypeKind::Vector(element, lanes) => checked_type_size(element)?.checked_mul(*lanes),
        TypeKind::String | TypeKind::Pointer(_) | TypeKind::Function(_, _) => Some(POINTER_SIZE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, builder_trait::*};

    #[test]
    fn computes_type_driven_strides_for_each_gep_index() {
        let row_ty = Type::get_array(Type::get_i32(), 32);
        let matrix_ty = Type::get_array(row_ty, 4);
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "gep_stride".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.new_local_inst().alloc(matrix_ty);
        let zero = data.new_local_inst().integer(0);
        let row = data.new_local_inst().integer(1);
        let column = data.new_local_inst().integer(2);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, row, column]);
        let ret = data.new_local_inst().ret(None);
        for inst in [base, gep, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        let data = program.func_data(function);
        assert_eq!(
            gep_index_stride(data, gep, 0),
            Some(GepIndexStride {
                byte_stride: 512,
                result_element_stride: 128,
            })
        );
        assert_eq!(
            gep_index_stride(data, gep, 1),
            Some(GepIndexStride {
                byte_stride: 128,
                result_element_stride: 32,
            })
        );
        assert_eq!(
            gep_index_stride(data, gep, 2),
            Some(GepIndexStride {
                byte_stride: 4,
                result_element_stride: 1,
            })
        );
    }
}
