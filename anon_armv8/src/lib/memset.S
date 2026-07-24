.section .text
.p2align 2
// x0 = destination, w1 = fill byte, x2 = byte count.
// This compiler-private helper clobbers only caller-saved registers x8-x14.
.Lsoyo_memset:
        cbz     x2, .Lsoyo_memset_done
        cmp     x2, #8
        b.hi    .Lsoyo_memset_large
        cmp     x2, #4
        b.lo    .Lsoyo_memset_small
        and     w8, w1, #0xff
        mov     w9, #16843009
        mul     w8, w8, w9
        add     x9, x0, x2
        str     w8, [x0]
        stur    w8, [x9, #-4]
        ret
.Lsoyo_memset_large:
        and     w8, w1, #0xff
        mov     x9, #72340172838076673
        cmp     x2, #32
        mul     x8, x8, x9
        b.hi    .Lsoyo_memset_loop_setup
        cmp     x2, #16
        str     x8, [x0]
        b.lo    .Lsoyo_memset_large_tail
        add     x9, x0, x2
        str     x8, [x0, #8]
        stp     x8, x8, [x9, #-16]
        ret
.Lsoyo_memset_small:
        lsr     x9, x2, #1
        add     x8, x0, x2
        strb    w1, [x0]
        sturb   w1, [x8, #-1]
        strb    w1, [x0, x9]
.Lsoyo_memset_done:
        ret
.Lsoyo_memset_loop_setup:
        neg     w9, w0
        str     x8, [x0]
        and     x13, x9, #0x7
        sub     x9, x2, x13
        add     x10, x0, x13
        subs    x11, x9, #64
        b.lo    .Lsoyo_memset_tail_setup
        cmp     x11, #192
        b.lo    .Lsoyo_memset_loop
        lsr     x11, x11, #6
        add     x13, x13, x0
        add     x13, x13, #128
        add     x11, x11, #1
        and     x12, x11, #0x7fffffffffffffc
        lsl     x14, x12, #6
        sub     x9, x9, x14
        add     x10, x10, x14
        and     x14, x11, #0x7fffffffffffffc
.Lsoyo_memset_unrolled_loop:
        stp     x8, x8, [x13, #-128]
        subs    x14, x14, #4
        stp     x8, x8, [x13, #-64]
        stp     x8, x8, [x13]
        stp     x8, x8, [x13, #64]
        stp     x8, x8, [x13, #-112]
        stp     x8, x8, [x13, #-48]
        stp     x8, x8, [x13, #16]
        stp     x8, x8, [x13, #80]
        stp     x8, x8, [x13, #-96]
        stp     x8, x8, [x13, #-32]
        stp     x8, x8, [x13, #32]
        stp     x8, x8, [x13, #96]
        stp     x8, x8, [x13, #-80]
        stp     x8, x8, [x13, #-16]
        stp     x8, x8, [x13, #48]
        stp     x8, x8, [x13, #112]
        add     x13, x13, #256
        b.ne    .Lsoyo_memset_unrolled_loop
        cmp     x11, x12
        b.eq    .Lsoyo_memset_after_loop
.Lsoyo_memset_loop:
        sub     x9, x9, #64
        stp     x8, x8, [x10]
        stp     x8, x8, [x10, #16]
        cmp     x9, #63
        stp     x8, x8, [x10, #32]
        stp     x8, x8, [x10, #48]
        add     x10, x10, #64
        b.hi    .Lsoyo_memset_loop
.Lsoyo_memset_after_loop:
        cmp     x9, #8
        mov     x11, x10
        b.lo    .Lsoyo_memset_final_tail
.Lsoyo_memset_tail_setup:
        mov     w11, #7
        mov     x12, #-8
        sub     x11, x11, x9
        cmn     x11, #8
        csel    x11, x11, x12, hi
        add     x11, x11, x9
        cmp     x11, #24
        b.hs    .Lsoyo_memset_pair_loop_setup
        mov     x11, x10
        b       .Lsoyo_memset_word_loop
.Lsoyo_memset_large_tail:
        add     x9, x0, x2
        stur    x8, [x9, #-8]
        ret
.Lsoyo_memset_pair_loop_setup:
        lsr     x11, x11, #3
        add     x12, x11, #1
        and     x13, x12, #0x3ffffffffffffffc
        and     x14, x12, #0x3ffffffffffffffc
        lsl     x11, x13, #3
        sub     x9, x9, x11
        add     x11, x10, x11
        add     x10, x10, #16
.Lsoyo_memset_pair_loop:
        subs    x14, x14, #4
        stp     x8, x8, [x10, #-16]
        stp     x8, x8, [x10], #32
        b.ne    .Lsoyo_memset_pair_loop
        cmp     x12, x13
        b.eq    .Lsoyo_memset_final_tail
.Lsoyo_memset_word_loop:
        sub     x9, x9, #8
        str     x8, [x11], #8
        cmp     x9, #7
        b.hi    .Lsoyo_memset_word_loop
.Lsoyo_memset_final_tail:
        cbz     x9, .Lsoyo_memset_done
        add     x9, x11, x9
        stur    x8, [x9, #-8]
        ret
