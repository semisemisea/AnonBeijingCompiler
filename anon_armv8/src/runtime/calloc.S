.section .text
.p2align 2

// .Lsoyo_calloc(count, elem_size): allocate a zero-initialized buffer.
// The IR passes both arguments as 32-bit integers; zero-extend them for the
// C `calloc(size_t, size_t)` ABI (AAPCS64 reads the full 64-bit registers) and
// tail-call the statically linked glibc implementation.
.Lsoyo_calloc:
    uxtw    x0, w0
    uxtw    x1, w1
    b       calloc
