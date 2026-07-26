use crate::labels::TargetSymbol;

/// Runtime routines a backend calls by name and expects the linker to resolve
/// against libc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibCall {
    Memset,
}

impl TargetSymbol for LibCall {
    fn symbol(self) -> &'static str {
        match self {
            Self::Memset => "memset",
        }
    }
}
