    .text
    .globl _start
    .type _start, %function
_start:
    movz w0, #0
    movk w0, #0x3f80, lsl #16
    fmov s0, w0
    movz w0, #0
    movk w0, #0x4000, lsl #16
    fmov s1, w0
    bl float_main
    fmov w0, s0
    movz w1, #0
    movk w1, #0x42c0, lsl #16
    cmp w0, w1
    b.ne .Lfail
    bl block_main
    cmp w0, #42
    b.ne .Lfail
    bl int_main
    mov w1, #528
    cmp w0, w1
    cset w0, ne
    b .Lexit
.Lfail:
    mov w0, #1
.Lexit:
    mov x8, #93
    svc #0
