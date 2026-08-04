.section .text
.p2align 2

// Clear x0 up to the next 16-byte boundary with scalar byte stores.
// Consumes at most 15 bytes (callers guarantee x1 >= 16 on every use site
// except the tail path, which guards x1 >= 16 before invoking it).
// Updates x0/x1; clobbers x4/x5.
.macro  ALIGN16_HEAD
    and     x4, x0, #15
    cbz     x4, 99f
    mov     x5, #16
    sub     x4, x5, x4
98:
    strb    wzr, [x0], #1
    sub     x1, x1, #1
    subs    x4, x4, #1
    b.ne    98b
99:
.endm

.Lsoyo_memzero:
    mov     x2, x0
    cbz     x1, .Lsoyo_memzero_done

    // Tier 1: len < 16. Pure scalar strb/strh/str, unaligned-safe,
    // and never reads dczid_el0.
    cmp     x1, #16
    b.lo    .Lsoyo_memzero_tail_scalar

    movi    v0.16b, #0          // zero vector for stp q / str q

    // Tier 2: 16 <= len <= 64. Scalar head to 16B, then a hardcoded
    // stp q unroll (no loop): at most 2x stp q0,q0 (32B each) + 1x str q0.
    cmp     x1, #64
    b.hi    .Lsoyo_memzero_tier3
    ALIGN16_HEAD
.Lsoyo_memzero_t2_unroll:
    cmp     x1, #32
    b.lo    .Lsoyo_memzero_t2_16
    stp     q0, q0, [x0], #32
    sub     x1, x1, #32
    cmp     x1, #32
    b.lo    .Lsoyo_memzero_t2_16
    stp     q0, q0, [x0], #32
    sub     x1, x1, #32
.Lsoyo_memzero_t2_16:
    cmp     x1, #16
    b.lo    .Lsoyo_memzero_tail_scalar
    str     q0, [x0], #16
    sub     x1, x1, #16
    b       .Lsoyo_memzero_tail_scalar

    // Tier 3: 64 < len <= 256. Scalar head to 16B, small stp q loop,
    // shared q/scalar tail.
.Lsoyo_memzero_tier3:
    cmp     x1, #256
    b.hi    .Lsoyo_memzero_tier4
    ALIGN16_HEAD
.Lsoyo_memzero_tier3_loop:
    cmp     x1, #32
    b.lo    .Lsoyo_memzero_tail_q
    stp     q0, q0, [x0], #32
    sub     x1, x1, #32
    b       .Lsoyo_memzero_tier3_loop

    // Tier 4: len > 256. This is the only path allowed to read dczid_el0.
.Lsoyo_memzero_tier4:
    mrs     x3, dczid_el0
    tbnz    x3, #4, .Lsoyo_memzero_tier4_stp   // DZP=1: dc zva unavailable
    and     x3, x3, #0xf
    mov     x4, #4
    lsl     x3, x4, x3                         // x3 = block size (4 << BS)
    ALIGN16_HEAD
    // Align x0 to the cache-block boundary (exactly) with stp q / str q /
    // scalar, so every dc zva below targets a block-aligned address.
    sub     x4, x3, #1
    and     x5, x0, x4                         // x5 = offset into current block
    cbz     x5, .Lsoyo_memzero_tier4_zva
    sub     x4, x3, x5                         // x4 = exact bytes to boundary
.Lsoyo_memzero_tier4_blk:
    cbz     x4, .Lsoyo_memzero_tier4_zva
    cmp     x4, #32
    b.lo    .Lsoyo_memzero_tier4_blk16
    stp     q0, q0, [x0], #32
    sub     x4, x4, #32
    sub     x1, x1, #32
    b       .Lsoyo_memzero_tier4_blk
.Lsoyo_memzero_tier4_blk16:
    cmp     x4, #16
    b.lo    .Lsoyo_memzero_tier4_blks
    str     q0, [x0], #16
    sub     x4, x4, #16
    sub     x1, x1, #16
    b       .Lsoyo_memzero_tier4_blk
.Lsoyo_memzero_tier4_blks:
    strb    wzr, [x0], #1
    sub     x4, x4, #1
    sub     x1, x1, #1
    b       .Lsoyo_memzero_tier4_blk
.Lsoyo_memzero_tier4_zva:
    cmp     x1, x3
    b.lo    .Lsoyo_memzero_tail_q
    dc      zva, x0
    add     x0, x0, x3
    sub     x1, x1, x3
    b       .Lsoyo_memzero_tier4_zva

    // DZP=1 fallback: skip dc zva entirely, plain stp q loop.
.Lsoyo_memzero_tier4_stp:
    ALIGN16_HEAD
    b       .Lsoyo_memzero_tier3_loop

    // Tail: 16B-align first (x1 >= 16 checked before), stp q loop, str q
    // for a single 16B, then a fully unrolled scalar finish (< 16B).
.Lsoyo_memzero_tail_q:
    cmp     x1, #16
    b.lo    .Lsoyo_memzero_tail_scalar
    ALIGN16_HEAD
.Lsoyo_memzero_tail_q_loop:
    cmp     x1, #32
    b.lo    .Lsoyo_memzero_tail_16
    stp     q0, q0, [x0], #32
    sub     x1, x1, #32
    b       .Lsoyo_memzero_tail_q_loop
.Lsoyo_memzero_tail_16:
    cmp     x1, #16
    b.lo    .Lsoyo_memzero_tail_scalar
    str     q0, [x0], #16
    sub     x1, x1, #16
    b       .Lsoyo_memzero_tail_q_loop

    // Unrolled scalar finish for the last < 16 bytes (unaligned-safe).
.Lsoyo_memzero_tail_scalar:
    cbz     x1, .Lsoyo_memzero_done
    cmp     x1, #8
    b.lo    .Lsoyo_memzero_tail4
    str     xzr, [x0], #8
    sub     x1, x1, #8
    cbz     x1, .Lsoyo_memzero_done
.Lsoyo_memzero_tail4:
    cmp     x1, #4
    b.lo    .Lsoyo_memzero_tail2
    str     wzr, [x0], #4
    sub     x1, x1, #4
    cbz     x1, .Lsoyo_memzero_done
.Lsoyo_memzero_tail2:
    cmp     x1, #2
    b.lo    .Lsoyo_memzero_tail1
    strh    wzr, [x0], #2
    sub     x1, x1, #2
    cbz     x1, .Lsoyo_memzero_done
.Lsoyo_memzero_tail1:
    strb    wzr, [x0]
    b       .Lsoyo_memzero_done

.Lsoyo_memzero_done:
    mov     x0, x2
    ret
