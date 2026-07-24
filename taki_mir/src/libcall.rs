#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibCall {
    Memset,
}

impl LibCall {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Memset => "memset",
        }
    }
}
