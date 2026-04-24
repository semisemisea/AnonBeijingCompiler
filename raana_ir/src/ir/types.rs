use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum TypeKind {
    /// Unit/Void Type
    Unit,
    /// i32
    Int32,
    /// f32
    Float32,
    /// An array like [base; len]
    Array(Type, usize),
    /// Pointer to the base type
    Pointer(Type),
    /// Function with parameters' type and return type.
    Function(Vec<Type>, Type),
}

impl std::fmt::Display for TypeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeKind::Unit => write!(f, "()"),
            TypeKind::Int32 => write!(f, "i32"),
            TypeKind::Float32 => write!(f, "f32"),
            TypeKind::Array(base, len) => write!(f, "[{base}; {len}]"),
            TypeKind::Pointer(base) => write!(f, "*{base}"),
            TypeKind::Function(params, ret) => {
                write!(f, "(")?;
                if let Some((last, rest)) = params.split_last() {
                    for param in rest {
                        write!(f, "{param}, ")?;
                    }
                    write!(f, "{last}")?;
                }
                write!(f, ")")?;
                write!(f, " -> ")?;
                write!(f, "{ret}")
            }
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Type(Rc<TypeKind>);

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.as_ref())
    }
}

pub const POINTER_SIZE: usize = std::mem::size_of::<*const ()>();

impl Type {
    thread_local! {
        static POOL: RefCell<HashMap<TypeKind, Type>> = RefCell::new(HashMap::new());
    }

    pub fn kind(&self) -> &TypeKind {
        &self.0
    }

    fn get(base: TypeKind) -> Type {
        Self::POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            pool.get(&base).cloned().unwrap_or_else(|| {
                let t = Type(Rc::new(base.clone()));
                pool.insert(base, t.clone());
                t
            })
        })
    }

    pub fn get_i32() -> Type {
        Type::get(TypeKind::Int32)
    }

    pub fn get_f32() -> Type {
        Type::get(TypeKind::Float32)
    }

    pub fn get_unit() -> Type {
        Type::get(TypeKind::Unit)
    }

    pub fn get_pointer(base: Type) -> Type {
        Type::get(TypeKind::Pointer(base))
    }

    pub fn get_array(base: Type, len: usize) -> Type {
        assert!(len > 0);
        Type::get(TypeKind::Array(base, len))
    }

    pub fn get_function(args: Vec<Type>, ret: Type) -> Type {
        Type::get(TypeKind::Function(args, ret))
    }

    pub fn is_i32(&self) -> bool {
        matches!(self.0.as_ref(), TypeKind::Int32)
    }

    pub fn is_f32(&self) -> bool {
        matches!(self.0.as_ref(), TypeKind::Float32)
    }

    pub fn is_unit(&self) -> bool {
        matches!(self.0.as_ref(), TypeKind::Unit)
    }

    pub fn size(&self) -> usize {
        match self.0.as_ref() {
            TypeKind::Unit => 0,
            TypeKind::Int32 | TypeKind::Float32 => 4,
            TypeKind::Array(base, len) => base.size() * len,
            TypeKind::Pointer(..) | TypeKind::Function(..) => POINTER_SIZE,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn display_test() {
        let unit = Type::get_unit();
        assert_eq!("()", unit.to_string());

        let int32 = Type::get_i32();
        assert_eq!("i32", int32.to_string());

        let float32 = Type::get_f32();
        assert_eq!("f32", float32.to_string());

        let i32_10 = Type::get_array(Type::get_i32(), 10);
        assert_eq!("[i32; 10]", i32_10.to_string());

        let i32_10_10 = Type::get_array(Type::get_array(Type::get_i32(), 10), 10);
        assert_eq!("[[i32; 10]; 10]", i32_10_10.to_string());

        let i32_p = Type::get_pointer(Type::get_i32());
        assert_eq!("*i32", i32_p.to_string());

        let i32_10_p = Type::get_pointer(Type::get_array(Type::get_i32(), 10));
        assert_eq!("*[i32; 10]", i32_10_p.to_string());

        let f1 = Type::get_function(vec![], Type::get_unit());
        assert_eq!("() -> ()", f1.to_string());

        let f2 = Type::get_function(vec![Type::get_i32()], Type::get_f32());
        assert_eq!("(i32) -> f32", f2.to_string());

        let f3 = Type::get_function(
            vec![
                Type::get_array(Type::get_i32(), 5),
                Type::get_pointer(Type::get_f32()),
                Type::get_i32(),
            ],
            Type::get_f32(),
        );
        assert_eq!("([i32, 5], *f32, i32) -> f32", f3.to_string());
    }
}
