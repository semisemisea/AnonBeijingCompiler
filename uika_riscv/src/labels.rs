use taki_mir::libcall::LibCall;

/// RISC-V labels name IR entities plus the libc routines this backend calls;
/// see [`LibCall`].
pub type Label = taki_mir::labels::Label<LibCall>;
