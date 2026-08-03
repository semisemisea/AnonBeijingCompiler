.section .text
.p2align 2

.Lsoyo_memzero:
    mov     x2, x0
    cbz     x1, .Lsoyo_memzero_done
    // DCZID_EL0.DZP permits DC ZVA only when clear. Its low four bits encode
    // log2(words) per zeroed block, so the block size is 4 << BS bytes.
    mrs     x3, dczid_el0
    tbnz    x3, #4, .Lsoyo_memzero_byte_loop
    and     x3, x3, #0xf
    mov     x4, #4
    lsl     x3, x4, x3
    // Clear to the next ZVA boundary before issuing cache-block zeroes.
    sub     x4, x3, #1
    and     x4, x0, x4
    cbz     x4, .Lsoyo_memzero_zva_loop
    sub     x4, x3, x4
    cmp     x1, x4
    b.lo    .Lsoyo_memzero_byte_loop

.Lsoyo_memzero_align_loop:
    strb    wzr,[x0],#1
    sub     x1, x1, #1
    subs    x4, x4, #1
    b.ne    .Lsoyo_memzero_align_loop

.Lsoyo_memzero_zva_loop:
    cbz     x1, .Lsoyo_memzero_done
    cmp     x1, x3
    b.lo    .Lsoyo_memzero_byte_loop
    dc      zva, x0
    add     x0, x0, x3
    sub     x1, x1, x3
    b       .Lsoyo_memzero_zva_loop

.Lsoyo_memzero_byte_loop:
    strb    wzr,[x0],#1
    subs    x1, x1, #1
    b.ne    .Lsoyo_memzero_byte_loop

.Lsoyo_memzero_done:
    mov     x0, x2
    ret
