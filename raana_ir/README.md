# RaanaIR

`RaanaIR` is a linear IR form backed by instructions, basicblocks and functions.

```RaanaIR
declare func <name = getint, ret_ty = i32>

declare func <name = getch, ret_ty = i32>

declare func <name = getarray, ret_ty = i32, params = (%0: *i32)>

declare func <name = putint, ret_ty = (), params = (%1: i32)>

declare func <name = putch, ret_ty = (), params = (%2: i32)>

declare func <name = putarray, ret_ty = (), params = (%3: i32, %4: *i32)>

declare func <name = starttime, ret_ty = ()>

declare func <name = stoptime, ret_ty = ()>

define func <name = main, ret_ty = i32>: {
entry:
    %v_a = alloc <type = *i32, size = 8>
    store 10, %v_a
    %6 = load %v_a <type = i32, size = 4>
    %7 = eq 0, %6 <type = i32, size = 4>
    %8 = eq 0, %7 <type = i32, size = 4>
    %9 = eq 0, %8 <type = i32, size = 4>
    %10 = sub 0, %9 <type = i32, size = 4>
    br %10, then, else
then:
    store -1, %v_a
    jump end
else:
    store 0, %v_a
    jump end
end:
    %16 = load %v_a <type = i32, size = 4>
    ret %16
}
```

Each line between the basic block is an instruction. It will a have a return type and value. (`unit/()/void` type is omitted as output.)
It looks similar to 3AC(3 Address code) in most of time, except branch/jump instruction.

Basicblock is a set of instruction, which must start execution from beginning and return/jump to other basicblock at the end.
Learned from `KoopaIR`, we also represent `phi` function as basicblock parameter.
