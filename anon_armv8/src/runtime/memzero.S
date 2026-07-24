.section .text
.p2align 2

.Lsoyo_memzero:
    mov     x2, x0
    cbz     x1, .Lsoyo_memzero_done
.Lsoyo_memzero_small:
    cmp     x1, #64
    b.lt    .Lsoyo_memzero_tail
.Lsoyo_memzero_align:
    and     x3, x0, #63
    cbz     x3, .Lsoyo_memzero_zva_loop
    strb    wzr,[x0],#1
    sub     x1,x1,#1
    cbnz    x1,.Lsoyo_memzero_align
.Lsoyo_memzero_zva_loop:
    cmp     x1,#64
    b.lt    .Lsoyo_memzero_tail
    dc      zva,x0
    add     x0,x0,#64
    sub     x1,x1,#64
    b       .Lsoyo_memzero_zva_loop
.Lsoyo_memzero_tail:
    cbz     x1,.Lsoyo_memzero_done
.Lsoyo_memzero_tail_loop:
    strb    wzr,[x0],#1
    subs    x1,x1,#1
    b.ne    .Lsoyo_memzero_tail_loop
.Lsoyo_memzero_done:
    mov     x0,x2
    ret