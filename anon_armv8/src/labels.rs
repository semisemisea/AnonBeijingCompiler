use crate::runtime::EmbeddedSymbol;

/// AArch64 labels name IR entities plus the compiler-provided assembly blobs
/// this backend embeds; see [`EmbeddedSymbol`].
pub type Label = taki_mir::labels::Label<EmbeddedSymbol>;
