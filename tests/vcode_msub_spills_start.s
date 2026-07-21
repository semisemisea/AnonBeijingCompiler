    .text
    .globl _start
    .type _start, %function
_start:
    bl main
    cmp w0, #1
    cset w0, ne
    mov x8, #93
    svc #0
