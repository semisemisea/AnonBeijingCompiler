	.arch armv8-a
	.file	"sylib.c"
	.text
	.align	2
	.p2align 4,,11
	.type	in_get, %function
in_get:
.LFB47:
	adrp	x1, .LANCHOR0
	ldr	w0, [x1, #:lo12:.LANCHOR0]
	tbnz	w0, #31, .L2
	mov	w2, -1
	str	w2, [x1, #:lo12:.LANCHOR0]
	ret
	.p2align 2,,3
.L2:
	stp	x29, x30, [sp, -32]!
.LCFI0:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI1:
	adrp	x19, .LANCHOR1
	add	x20, x19, :lo12:.LANCHOR1
	ldr	x1, [x19, #:lo12:.LANCHOR1]
	ldr	x0, [x20, 8]
	cmp	x1, x0
	beq	.L10
.L4:
	cmp	x0, x1
	bls	.L5
	adrp	x0, in_buf
	add	x0, x0, :lo12:in_buf
	add	x2, x1, 1
	str	x2, [x19, #:lo12:.LANCHOR1]
	ldrb	w0, [x0, x1]
.L1:
	ldp	x19, x20, [sp, 16]
	ldp	x29, x30, [sp], 32
.LCFI2:
	ret
	.p2align 2,,3
.L10:
.LCFI3:
	adrp	x3, :got:stdin
	ldr	x3, [x3, :got_lo12:stdin]
	mov	x1, 1
	adrp	x0, in_buf
	mov	x2, 8192
	add	x0, x0, :lo12:in_buf
	ldr	x3, [x3]
	bl	fread
	str	xzr, [x19, #:lo12:.LANCHOR1]
	mov	x1, 0
	str	x0, [x20, 8]
	b	.L4
.L5:
	mov	w0, -1
	b	.L1
.LFE47:
	.size	in_get, .-in_get
	.align	2
	.p2align 4,,11
	.type	format_hex_float, %function
format_hex_float:
.LFB45:
	fmov	w1, s0
	mov	x5, x0
	ubfx	x0, x1, 23, 8
	and	w2, w1, 8388607
	lsr	w1, w1, 31
	cmp	w0, 255
	beq	.L46
	mov	x7, x5
	cbz	w1, .L16
	mov	w1, 45
	strb	w1, [x7], 1
.L16:
	mov	w1, 30768
	strh	w1, [x7]
	orr	w1, w2, w0
	cbz	w1, .L47
	sub	w6, w0, #127
	cbnz	w0, .L21
	mov	w1, 23
	.p2align 3,,7
.L20:
	mov	w6, w1
	subs	w1, w1, #1
	beq	.L34
	lsr	w0, w2, w1
	tbz	x0, 0, .L20
	mov	w0, 23
	sub	w6, w6, #150
	sub	w0, w0, w1
.L19:
	lsl	w2, w2, w0
	and	w2, w2, 8388607
.L21:
	add	x9, x7, 3
	adrp	x8, .LANCHOR2
	lsl	w3, w2, 1
	mov	x4, x9
	add	x8, x8, :lo12:.LANCHOR2
	mov	w0, 49
	mov	w1, 20
	strb	w0, [x7, 2]
	.p2align 3,,7
.L22:
	lsr	w2, w3, w1
	and	w2, w2, 15
	sub	w1, w1, #4
	ldrb	w0, [x8, w2, sxtw]
	strb	w0, [x4], 1
	cmn	w1, #4
	bne	.L22
	add	x2, x7, 9
	mov	w1, 6
	b	.L23
	.p2align 2,,3
.L25:
	subs	w1, w1, #1
	beq	.L24
.L23:
	ldrb	w3, [x2, -1]
	sub	x2, x2, #1
	cmp	w3, 48
	beq	.L25
	sxtw	x1, w1
	add	x0, x7, x1
.L27:
	ldrb	w2, [x0, 2]
	sub	x0, x0, #1
	strb	w2, [x0, 4]
	cmp	x7, x0
	bne	.L27
	add	x1, x1, 4
	mov	w0, 46
	add	x9, x7, x1
	strb	w0, [x7, 3]
.L24:
	mov	w0, 112
	strb	w0, [x9]
	mov	w0, 43
	tbz	w6, #31, .L28
	neg	w6, w6
	mov	w0, 45
.L28:
	strb	w0, [x9, 1]
	cmp	w6, 99
	ble	.L29
	mov	w2, 34079
	mov	w3, 100
	movk	w2, 0x51eb, lsl 16
	mov	w1, 52429
	movk	w1, 0xcccc, lsl 16
	add	x0, x9, 5
	umull	x2, w6, w2
	sub	x0, x0, x5
	mov	w4, 49
	strb	w4, [x9, 2]
	lsr	x2, x2, 37
	msub	w2, w2, w3, w6
	umull	x1, w2, w1
	lsr	x1, x1, 35
	add	w3, w1, 48
	strb	w3, [x9, 3]
	add	w1, w1, w1, lsl 2
	sub	w1, w2, w1, lsl 1
	add	w1, w1, 48
	strb	w1, [x9, 4]
.L11:
	ret
	.p2align 2,,3
.L47:
	add	x0, x7, 6
	mov	w1, 28720
	movk	w1, 0x302b, lsl 16
	sub	x0, x0, x5
	str	w1, [x7, 2]
	ret
	.p2align 2,,3
.L46:
	cbnz	w2, .L48
	mov	x2, x5
	cbz	w1, .L15
	mov	w0, 45
	strb	w0, [x2], 1
.L15:
	add	x0, x2, 3
	mov	w3, 28265
	mov	w1, 102
	sub	x0, x0, x5
	strh	w3, [x2]
	strb	w1, [x2, 2]
	ret
	.p2align 2,,3
.L29:
	cmp	w6, 9
	ble	.L31
	mov	w1, 52429
	add	x0, x9, 4
	movk	w1, 0xcccc, lsl 16
	sub	x0, x0, x5
	umull	x1, w6, w1
	lsr	x1, x1, 35
	add	w2, w1, 48
	strb	w2, [x9, 2]
	add	w1, w1, w1, lsl 2
	sub	w1, w6, w1, lsl 1
	add	w1, w1, 48
	strb	w1, [x9, 3]
	b	.L11
	.p2align 2,,3
.L31:
	add	x0, x9, 3
	add	w6, w6, 48
	sub	x0, x0, x5
	strb	w6, [x9, 2]
	b	.L11
	.p2align 2,,3
.L48:
	mov	w2, 24942
	mov	w1, 110
	mov	x0, 3
	strh	w2, [x5]
	strb	w1, [x5, 2]
	ret
	.p2align 2,,3
.L34:
	mov	w6, -149
	mov	w0, 23
	b	.L19
.LFE45:
	.size	format_hex_float, .-format_hex_float
	.align	2
	.p2align 4,,11
	.type	format_dec, %function
format_dec:
.LFB43:
	cmp	w1, 0
	mov	x8, x0
	csneg	w2, w1, w1, ge
	add	x0, x0, 12
	cmp	w2, 99
	bls	.L60
	adrp	x6, .LANCHOR2
	add	x6, x6, :lo12:.LANCHOR2
	mov	w11, 34079
	add	x6, x6, 32
	mov	x5, x0
	movk	w11, 0x51eb, lsl 16
	mov	w10, 100
	mov	w9, 9999
	.p2align 3,,7
.L52:
	umull	x4, w2, w11
	mov	w7, w2
	lsr	x4, x4, 37
	msub	w3, w4, w10, w2
	mov	w2, w4
	lsl	w3, w3, 1
	add	w4, w3, 1
	ldrb	w3, [x6, w3, uxtw]
	ldrb	w4, [x6, w4, uxtw]
	strb	w4, [x5, -1]
	strb	w3, [x5, -2]!
	cmp	w7, w9
	bhi	.L52
.L51:
	cmp	w2, 9
	bls	.L53
	lsl	w2, w2, 1
	adrp	x4, .LANCHOR2
	add	x4, x4, :lo12:.LANCHOR2
	add	w7, w2, 1
	add	x4, x4, 32
	sub	x3, x5, #2
	ldrb	w6, [x4, w2, uxtw]
	ldrb	w2, [x4, w7, uxtw]
	strb	w6, [x5, -2]
.L54:
	strb	w2, [x5, -1]
	tbz	w1, #31, .L55
	mov	w1, 45
	sub	x3, x3, #1
	strb	w1, [x3]
.L55:
	sub	x0, x0, x3
	cmp	x3, x8
	bcs	.L56
	mov	x1, x0
	cbz	x0, .L49
	.p2align 3,,7
.L57:
	sub	x1, x1, #1
	ldrb	w2, [x3, x1]
	strb	w2, [x8, x1]
	cbnz	x1, .L57
.L49:
	ret
	.p2align 2,,3
.L56:
	cbz	x0, .L49
	mov	x1, 0
	.p2align 3,,7
.L59:
	ldrb	w2, [x3, x1]
	strb	w2, [x8, x1]
	add	x1, x1, 1
	cmp	x0, x1
	bne	.L59
	ret
	.p2align 2,,3
.L53:
	add	w2, w2, 48
	sub	x3, x5, #1
	and	w2, w2, 255
	b	.L54
	.p2align 2,,3
.L60:
	mov	x5, x0
	b	.L51
.LFE43:
	.size	format_dec, .-format_dec
	.align	2
	.p2align 4,,11
	.type	out_str, %function
out_str:
.LFB41:
	cbz	x1, .L81
	stp	x29, x30, [sp, -80]!
.LCFI4:
	mov	x29, sp
	stp	x21, x22, [sp, 32]
.LCFI5:
	adrp	x22, .LANCHOR1
	add	x22, x22, :lo12:.LANCHOR1
	mov	x21, x1
	stp	x19, x20, [sp, 16]
.LCFI6:
	mov	x19, x0
	ldr	x20, [x22, 16]
	stp	x23, x24, [sp, 48]
.LCFI7:
	mov	x23, 8192
	str	x25, [sp, 64]
.LCFI8:
	adrp	x25, out_buf
	add	x24, x25, :lo12:out_buf
	.p2align 3,,7
.L73:
	cmp	x20, 8192
	beq	.L71
	sub	x5, x23, x20
	cmp	x5, x21
	csel	x5, x5, x21, ls
.L72:
	add	x0, x20, x24
	mov	x3, 0
	.p2align 3,,7
.L75:
	ldrb	w4, [x19, x3]
	strb	w4, [x0, x3]
	add	x3, x3, 1
	cmp	x5, x3
	bne	.L75
	add	x20, x20, x5
	str	x20, [x22, 16]
	add	x19, x19, x5
	subs	x21, x21, x5
	bne	.L73
	ldp	x19, x20, [sp, 16]
	ldp	x21, x22, [sp, 32]
	ldp	x23, x24, [sp, 48]
	ldr	x25, [sp, 64]
	ldp	x29, x30, [sp], 80
.LCFI9:
	ret
	.p2align 2,,3
.L71:
.LCFI10:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x2, x20
	add	x0, x25, :lo12:out_buf
	mov	x1, 1
	ldr	x3, [x3]
	bl	fwrite
	cmp	x21, 8192
	csel	x5, x21, x20, ls
	mov	x20, 0
	b	.L72
.L81:
.LCFI11:
	ret
.LFE41:
	.size	out_str, .-out_str
	.align	2
	.p2align 4,,11
	.type	out_char, %function
out_char:
.LFB40:
	stp	x29, x30, [sp, -48]!
.LCFI12:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI13:
	adrp	x19, .LANCHOR1
	add	x1, x19, :lo12:.LANCHOR1
	ldr	x2, [x1, 16]
	str	x21, [sp, 32]
.LCFI14:
	and	w21, w0, 255
	cmp	x2, 8192
	beq	.L85
	add	x1, x2, 1
	adrp	x20, out_buf
.L86:
	add	x0, x20, :lo12:out_buf
	add	x19, x19, :lo12:.LANCHOR1
	strb	w21, [x0, x2]
	ldr	x21, [sp, 32]
	str	x1, [x19, 16]
	ldp	x19, x20, [sp, 16]
	ldp	x29, x30, [sp], 48
.LCFI15:
	ret
	.p2align 2,,3
.L85:
.LCFI16:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x1, 1
	adrp	x20, out_buf
	add	x0, x20, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	mov	x1, 1
	mov	x2, 0
	b	.L86
.LFE40:
	.size	out_char, .-out_char
	.align	2
	.p2align 4,,11
	.global	getint
	.type	getint, %function
getint:
.LFB50:
	stp	x29, x30, [sp, -32]!
.LCFI17:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI18:
	.p2align 3,,7
.L90:
	bl	in_get
	tbnz	w0, #31, .L99
	cmp	w0, 32
	sub	w1, w0, #9
	ccmp	w1, 4, 0, ne
	bls	.L90
	cmp	w0, 45
	bne	.L91
	mov	w20, 1
	bl	in_get
	b	.L89
	.p2align 2,,3
.L99:
	mov	w20, 0
.L89:
	sub	w1, w0, #48
	cmp	w1, 9
	bhi	.L101
.L93:
	sub	w1, w0, #48
	mov	w19, 0
	.p2align 3,,7
.L96:
	add	w19, w19, w19, lsl 2
	add	w19, w1, w19, lsl 1
	bl	in_get
	sub	w1, w0, #48
	cmp	w1, 9
	bls	.L96
.L95:
	tbnz	w0, #31, .L97
.L94:
	adrp	x1, .LANCHOR0
	str	w0, [x1, #:lo12:.LANCHOR0]
.L97:
	cmp	w20, 0
	csneg	w0, w19, w19, eq
	ldp	x19, x20, [sp, 16]
	ldp	x29, x30, [sp], 32
.LCFI19:
	ret
	.p2align 2,,3
.L91:
.LCFI20:
	cmp	w0, 43
	bne	.L107
	mov	w20, 0
	bl	in_get
	b	.L89
	.p2align 2,,3
.L107:
	sub	w1, w0, #48
	mov	w20, 0
	mov	w19, 0
	cmp	w1, 9
	bls	.L93
	b	.L94
	.p2align 2,,3
.L101:
	mov	w19, 0
	b	.L95
.LFE50:
	.size	getint, .-getint
	.align	2
	.p2align 4,,11
	.global	getch
	.type	getch, %function
getch:
.LFB51:
	adrp	x1, .LANCHOR0
	ldr	w0, [x1, #:lo12:.LANCHOR0]
	tbnz	w0, #31, .L109
	mov	w2, -1
	str	w2, [x1, #:lo12:.LANCHOR0]
	ret
	.p2align 2,,3
.L109:
	stp	x29, x30, [sp, -32]!
.LCFI21:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI22:
	adrp	x19, .LANCHOR1
	add	x20, x19, :lo12:.LANCHOR1
	ldr	x1, [x19, #:lo12:.LANCHOR1]
	ldr	x0, [x20, 8]
	cmp	x1, x0
	beq	.L116
.L111:
	cmp	x0, x1
	bls	.L112
	adrp	x0, in_buf
	add	x0, x0, :lo12:in_buf
	add	x2, x1, 1
	str	x2, [x19, #:lo12:.LANCHOR1]
	ldrb	w0, [x0, x1]
.L108:
	ldp	x19, x20, [sp, 16]
	ldp	x29, x30, [sp], 32
.LCFI23:
	ret
	.p2align 2,,3
.L116:
.LCFI24:
	adrp	x3, :got:stdin
	ldr	x3, [x3, :got_lo12:stdin]
	mov	x1, 1
	adrp	x0, in_buf
	mov	x2, 8192
	add	x0, x0, :lo12:in_buf
	ldr	x3, [x3]
	bl	fread
	str	xzr, [x19, #:lo12:.LANCHOR1]
	mov	x1, 0
	str	x0, [x20, 8]
	b	.L111
.L112:
	mov	w0, 0
	b	.L108
.LFE51:
	.size	getch, .-getch
	.align	2
	.p2align 4,,11
	.global	getfloat
	.type	getfloat, %function
getfloat:
.LFB54:
	sub	sp, sp, #112
.LCFI25:
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	stp	x29, x30, [sp, 80]
.LCFI26:
	add	x29, sp, 80
	stp	x19, x20, [sp, 96]
.LCFI27:
	ldr	x1, [x0]
	str	x1, [sp, 72]
	mov	x1, 0
	.p2align 3,,7
.L119:
	bl	in_get
	tbnz	w0, #31, .L118
	cmp	w0, 32
	sub	w1, w0, #9
	ccmp	w1, 4, 0, ne
	bls	.L119
	add	x20, sp, 8
	mov	x19, 0
.L120:
	add	x1, x19, 1
	cmp	x1, 63
	bhi	.L126
	strb	w0, [x20, x19]
	mov	x19, x1
.L126:
	bl	in_get
	tbnz	w0, #31, .L122
	cmp	w0, 32
	sub	w1, w0, #9
	ccmp	w1, 4, 0, ne
	bhi	.L120
	adrp	x1, .LANCHOR0
	str	w0, [x1, #:lo12:.LANCHOR0]
.L122:
	ldrb	w4, [sp, 8]
	strb	wzr, [x20, x19]
	sub	w0, w4, #43
	and	w0, w0, -3
	ands	w1, w0, 255
	beq	.L208
	mov	w2, w4
	mov	x3, x20
	orr	w0, w2, 32
	cmp	w0, 105
	beq	.L279
.L127:
	cmp	w0, 110
	bne	.L128
	ldrb	w0, [x3, 1]
	orr	w0, w0, 32
	cmp	w0, 97
	beq	.L280
.L128:
	cmp	w2, 48
	bne	.L130
	ldrb	w0, [x3, 1]
	orr	w0, w0, 32
	cmp	w0, 120
	beq	.L281
.L130:
	cmp	w4, 45
	beq	.L282
	mov	x2, x20
	mov	w11, 0
	cbz	w1, .L283
.L179:
	sub	w1, w4, #48
	mov	w13, 0
	and	w0, w1, 255
	mov	w9, 0
	cmp	w0, 9
	mov	x3, 0
	mov	w8, 0
	mov	w10, 0
	mov	w0, 0
	bls	.L184
	b	.L180
	.p2align 2,,3
.L284:
	cmp	w8, 18
	ble	.L182
	add	w9, w9, 1
.L183:
	ldrb	w4, [x2, 1]!
	sub	w1, w4, #48
	and	w5, w1, 255
	cmp	w5, 9
	bhi	.L180
.L184:
	orr	w0, w0, w1
	cbz	w0, .L284
	add	w10, w10, 1
	cmp	w8, 18
	ble	.L222
	add	w9, w9, 1
	mov	w0, 1
	cbz	w1, .L183
	ldrb	w4, [x2, 1]!
	mov	w13, w0
	sub	w1, w4, #48
	and	w5, w1, 255
	cmp	w5, 9
	bls	.L184
	.p2align 3,,7
.L180:
	cmp	w4, 46
	beq	.L285
	ldrb	w4, [x2]
	mov	w6, 0
	mov	x7, 0
.L186:
	and	w4, w4, -33
	mov	w0, 0
	cmp	w4, 69
	bne	.L191
	ldrb	w5, [x2, 1]
	cmp	w5, 45
	beq	.L286
	cmp	w5, 43
	beq	.L287
	sub	w4, w5, #48
	and	w1, w4, 255
	cmp	w1, 9
	bhi	.L191
	add	x2, x2, 1
	mov	w14, 0
.L193:
	mov	w12, 34463
	mov	w0, 0
	movk	w12, 0x1, lsl 16
	.p2align 3,,7
.L197:
	ldrb	w5, [x2, 1]!
	add	w1, w0, w0, lsl 2
	cmp	w0, w12
	add	w1, w4, w1, lsl 1
	sub	w4, w5, #48
	and	w5, w4, 255
	csel	w0, w1, w0, le
	cmp	w5, 9
	bls	.L197
	cmp	w14, 0
	csneg	w0, w0, w0, eq
.L191:
	cbz	w8, .L118
	cmp	w10, 9
	cset	w1, gt
	orr	w1, w1, w13
	cbnz	w1, .L288
	ucvtf	d0, x3
	add	w0, w9, w0
	cmp	w6, 0
	cbz	w6, .L199
	ble	.L231
	mov	w1, 0
	fmov	d2, 1.0e+0
	fmov	d1, 1.0e+1
	.p2align 3,,7
.L201:
	add	w1, w1, 1
	fmul	d2, d2, d1
	cmp	w6, w1
	bne	.L201
.L200:
	ucvtf	d1, x7
	fdiv	d1, d1, d2
	fadd	d0, d0, d1
.L199:
	cmp	w0, 308
	bgt	.L289
	cmn	w0, #324
	blt	.L290
	cmp	w0, 0
	ble	.L204
	mov	w1, 0
	fmov	d1, 1.0e+1
	.p2align 3,,7
.L205:
	add	w1, w1, 1
	fmul	d0, d0, d1
	cmp	w0, w1
	bne	.L205
.L206:
	fcvt	s0, d0
	cmp	w11, 0
	fneg	s1, s0
	fcsel	s0, s1, s0, ne
	b	.L117
	.p2align 2,,3
.L208:
	ldrb	w2, [sp, 9]
	add	x3, sp, 9
	orr	w0, w2, 32
	cmp	w0, 105
	bne	.L127
.L279:
	ldrb	w0, [x3, 1]
	orr	w0, w0, 32
	cmp	w0, 110
	bne	.L128
	ldrb	w0, [x3, 2]
	mov	w2, 2139095040
	orr	w0, w0, 32
	cmp	w0, 102
	bne	.L130
.L129:
	cmp	w4, 45
	cset	w0, eq
	orr	w0, w2, w0, lsl 31
	fmov	s0, w0
	b	.L117
	.p2align 2,,3
.L222:
	mov	w0, 1
.L182:
	add	x3, x3, x3, lsl 2
	sxtw	x1, w1
	add	w8, w8, 1
	add	x3, x1, x3, lsl 1
	b	.L183
	.p2align 2,,3
.L118:
	movi	v0.2s, #0
.L117:
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	ldr	x2, [sp, 72]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L291
	ldp	x29, x30, [sp, 80]
	ldp	x19, x20, [sp, 96]
	add	sp, sp, 112
.LCFI28:
	ret
.L283:
.LCFI29:
	ldrb	w4, [sp, 9]
	add	x2, sp, 9
	b	.L179
.L280:
	ldrb	w0, [x3, 2]
	orr	w0, w0, 32
	cmp	w0, 110
	bne	.L130
	mov	w2, 2143289344
	b	.L129
.L289:
	lsl	w0, w11, 31
	orr	w0, w0, 2139095040
	fmov	s0, w0
	b	.L117
.L286:
	ldrb	w5, [x2, 2]
	mov	w14, 1
	add	x2, x2, 2
	sub	w4, w5, #48
	and	w1, w4, 255
	cmp	w1, 9
	bls	.L193
	b	.L191
	.p2align 2,,3
.L285:
	ldrb	w4, [x2, 1]
	add	x2, x2, 1
	sub	w1, w4, #48
	and	w5, w1, 255
	cmp	w5, 9
	bhi	.L224
	mov	x7, 0
	mov	w6, 0
	b	.L190
	.p2align 2,,3
.L293:
	cmp	w6, 18
	bgt	.L189
.L188:
	add	x7, x7, x7, lsl 2
	sxtw	x1, w1
	add	w6, w6, 1
	add	x7, x1, x7, lsl 1
.L189:
	ldrb	w4, [x2, 1]!
	sub	w1, w4, #48
	and	w5, w1, 255
	cmp	w5, 9
	bhi	.L292
.L190:
	orr	w0, w0, w1
	cbz	w0, .L293
	add	w10, w10, 1
	mov	w0, 1
	cmp	w6, 18
	ble	.L188
	cbz	w1, .L189
	ldrb	w4, [x2, 1]!
	mov	w13, w0
	sub	w1, w4, #48
	and	w5, w1, 255
	cmp	w5, 9
	bls	.L190
.L292:
	orr	w8, w8, w6
	b	.L186
	.p2align 2,,3
.L282:
	ldrb	w4, [sp, 9]
	add	x2, sp, 9
	mov	w11, 1
	b	.L179
.L288:
	mov	x0, x20
	mov	x1, 0
	bl	strtof
	b	.L117
.L281:
	cmp	w4, 45
	beq	.L294
	mov	w0, 0
	cbnz	w1, .L132
	ldrb	w4, [sp, 9]
	add	x20, sp, 9
.L132:
	cmp	w4, 48
	bne	.L118
	ldrb	w1, [x20, 1]
	orr	w1, w1, 32
	cmp	w1, 120
	bne	.L118
	add	x1, x20, 2
	mov	w9, 0
	mov	w7, -1
	mov	w2, 0
	mov	w6, 0
	mov	x4, 0
	mov	w8, 1
	b	.L145
.L133:
	sub	w5, w3, #97
	cmp	w5, 5
	bls	.L295
	sub	w5, w3, #65
	cmp	w5, 5
	bhi	.L139
	sub	w3, w3, #55
	tbnz	w7, #31, .L137
.L271:
	cmp	w6, 9
	bgt	.L296
.L140:
	sxtw	x3, w3
	add	w6, w6, 1
	add	x4, x3, x4, lsl 4
.L141:
	add	w2, w2, 1
	add	x1, x1, 1
.L145:
	ldrb	w3, [x1]
	sub	w5, w3, #48
	cmp	w5, 9
	bhi	.L133
	mov	w3, w5
	tbz	w7, #31, .L297
	cbz	w5, .L141
.L137:
	sxtw	x4, w3
	mov	w7, w2
	mov	w6, 1
	b	.L141
.L290:
	fmov	s0, w11
	shl	v0.2s, v0.2s, 31
	b	.L117
.L287:
	ldrb	w5, [x2, 2]
	mov	w14, 0
	add	x2, x2, 2
	sub	w4, w5, #48
	and	w1, w4, 255
	cmp	w1, 9
	bls	.L193
	b	.L191
.L204:
	beq	.L206
	neg	w0, w0
	mov	w1, 0
	fmov	d1, 1.0e+1
	.p2align 3,,7
.L207:
	fdiv	d0, d0, d1
	add	w1, w1, 1
	cmp	w0, w1
	bne	.L207
	b	.L206
.L224:
	mov	w6, 0
	mov	x7, 0
	b	.L186
.L295:
	sub	w3, w3, #87
	tbz	w7, #31, .L271
	b	.L137
.L231:
	fmov	d2, 1.0e+0
	b	.L200
.L294:
	ldrb	w4, [sp, 9]
	add	x20, sp, 9
	mov	w0, 1
	b	.L132
.L296:
	mov	w9, 1
	b	.L141
.L297:
	cmp	w6, 9
	ble	.L140
	cmp	w5, 0
	csel	w9, w9, w8, eq
	b	.L141
.L139:
	cmp	w3, 46
	beq	.L298
.L143:
	fmov	s0, w0
	shl	v0.2s, v0.2s, 31
	tbnz	w7, #31, .L117
	ldrb	w5, [x1]
	mov	w3, 0
	and	w5, w5, -33
	and	w5, w5, 255
	cmp	w5, 80
	bne	.L157
	ldrb	w8, [x1, 1]
	cmp	w8, 45
	beq	.L299
	cmp	w8, 43
	beq	.L300
	sub	w5, w8, #48
	and	w8, w5, 255
	cmp	w8, 9
	bhi	.L157
	add	x1, x1, 1
	mov	w11, 0
.L159:
	mov	w12, 34463
	mov	w3, 0
	movk	w12, 0x1, lsl 16
.L163:
	ldrb	w8, [x1, 1]!
	add	w10, w3, w3, lsl 2
	cmp	w3, w12
	add	w10, w5, w10, lsl 1
	sub	w5, w8, #48
	and	w8, w5, 255
	csel	w3, w10, w3, le
	cmp	w8, 9
	bls	.L163
	cbz	w11, .L157
	neg	w3, w3
.L157:
	sub	w1, w2, w7
	clz	x7, x4
	sub	w1, w1, w6
	add	w1, w3, w1, lsl 2
	sub	w1, w1, w7
	add	w1, w1, 190
	cmp	w1, 254
	bgt	.L177
	sub	w5, w7, #40
	cmp	w1, 0
	ble	.L165
	tbnz	w5, #31, .L301
	lsl	x2, x4, x5
	mov	x3, 16777216
	cmp	x2, x3
	bne	.L302
.L171:
	add	w2, w1, 1
	cmp	w1, 254
	beq	.L177
	lsl	w1, w2, 23
	mov	x2, 8388608
.L169:
	orr	w0, w1, w0, lsl 31
	and	w2, w2, 8388607
	orr	w0, w0, w2
	fmov	s0, w0
	b	.L117
.L298:
	add	x1, x1, 1
	mov	w10, 1
	sub	w8, w2, w1
.L155:
	ldrb	w3, [x1]
	sub	w5, w3, #48
	cmp	w5, 9
	bls	.L303
	sub	w5, w3, #97
	cmp	w5, 5
	bls	.L304
	sub	w5, w3, #65
	cmp	w5, 5
	bhi	.L143
	sub	w3, w3, #55
	tbnz	w7, #31, .L150
.L273:
	cmp	w6, 9
	bgt	.L305
.L152:
	sxtw	x3, w3
	add	w6, w6, 1
	add	x4, x3, x4, lsl 4
.L153:
	add	x1, x1, 1
	b	.L155
.L303:
	mov	w3, w5
	tbz	w7, #31, .L306
	cbz	w5, .L153
.L150:
	add	w7, w8, w1
	sxtw	x4, w3
	add	x1, x1, 1
	mov	w6, 1
	b	.L155
.L304:
	sub	w3, w3, #87
	tbz	w7, #31, .L273
	b	.L150
.L306:
	cmp	w6, 9
	ble	.L152
	cmp	w5, 0
	add	x1, x1, 1
	csel	w9, w9, w10, eq
	b	.L155
.L305:
	add	x1, x1, 1
	mov	w9, 1
	b	.L155
.L177:
	lsl	w0, w0, 31
	orr	w0, w0, 2139095040
	fmov	s0, w0
	b	.L117
.L300:
	ldrb	w8, [x1, 2]
	mov	w11, 0
	add	x1, x1, 2
	sub	w5, w8, #48
	and	w8, w5, 255
	cmp	w8, 9
	bls	.L159
	b	.L157
.L299:
	ldrb	w8, [x1, 2]
	mov	w11, 1
	add	x1, x1, 2
	sub	w5, w8, #48
	and	w8, w5, 255
	cmp	w8, 9
	bls	.L159
	neg	w3, w3
	b	.L157
	.p2align 2,,3
.L165:
	mov	w2, 1
	sub	w2, w2, w1
	subs	w3, w5, w2
	bmi	.L168
	lsl	x2, x4, x3
	mov	x1, 16777216
	cmp	x2, x1
	beq	.L217
	mov	w1, 0
.L170:
	cmp	w1, 0
	mov	x3, 8388607
	lsl	w1, w1, 23
	ccmp	x2, x3, 0, eq
	mov	w3, 8388608
	csel	w1, w1, w3, ls
	b	.L169
.L301:
	mov	w2, 40
	mov	w3, w5
	sub	w2, w2, w7
.L167:
	mov	x6, -1
	mvn	w3, w3
	lsl	x5, x6, x2
	bic	x5, x4, x5
	lsr	x2, x4, x2
	lsr	x4, x5, x3
	and	w4, w4, 1
	cbnz	w9, .L173
	lsl	x6, x6, x3
	bics	xzr, x5, x6
	bne	.L173
	cbz	w4, .L176
	tbnz	x2, 0, .L175
.L176:
	mov	x3, 16777216
	cmp	x2, x3
	bne	.L170
	b	.L171
.L173:
	cbz	w4, .L176
.L175:
	add	x2, x2, 1
	b	.L176
.L168:
	cmn	w3, #40
	bge	.L172
	cbz	w0, .L118
	movi	v0.2s, 0x80, lsl 24
	b	.L117
.L217:
	mov	w1, 8388608
	mov	x2, 8388608
	b	.L169
.L172:
	sub	w2, w2, w5
	mov	w1, 0
	b	.L167
.L291:
	bl	__stack_chk_fail
.L302:
	lsl	w1, w1, 23
	b	.L169
.LFE54:
	.size	getfloat, .-getfloat
	.align	2
	.p2align 4,,11
	.global	getarray
	.type	getarray, %function
getarray:
.LFB55:
	stp	x29, x30, [sp, -48]!
.LCFI30:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI31:
	mov	x19, x0
	str	x21, [sp, 32]
.LCFI32:
	bl	getint
	mov	w21, w0
	cmp	w0, 0
	ble	.L307
	add	x20, x19, w0, sxtw 2
	.p2align 3,,7
.L309:
	bl	getint
	str	w0, [x19], 4
	cmp	x19, x20
	bne	.L309
.L307:
	ldp	x19, x20, [sp, 16]
	mov	w0, w21
	ldr	x21, [sp, 32]
	ldp	x29, x30, [sp], 48
.LCFI33:
	ret
.LFE55:
	.size	getarray, .-getarray
	.align	2
	.p2align 4,,11
	.global	getfarray
	.type	getfarray, %function
getfarray:
.LFB56:
	stp	x29, x30, [sp, -48]!
.LCFI34:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI35:
	mov	x19, x0
	str	x21, [sp, 32]
.LCFI36:
	bl	getint
	mov	w21, w0
	cmp	w0, 0
	ble	.L312
	add	x20, x19, w0, sxtw 2
	.p2align 3,,7
.L314:
	bl	getfloat
	str	s0, [x19], 4
	cmp	x19, x20
	bne	.L314
.L312:
	ldp	x19, x20, [sp, 16]
	mov	w0, w21
	ldr	x21, [sp, 32]
	ldp	x29, x30, [sp], 48
.LCFI37:
	ret
.LFE56:
	.size	getfarray, .-getfarray
	.align	2
	.p2align 4,,11
	.global	putint
	.type	putint, %function
putint:
.LFB57:
	sub	sp, sp, #48
.LCFI38:
	mov	w1, w0
	adrp	x2, :got:__stack_chk_guard
	ldr	x2, [x2, :got_lo12:__stack_chk_guard]
	add	x12, sp, 8
	stp	x29, x30, [sp, 32]
.LCFI39:
	add	x29, sp, 32
	ldr	x0, [x2]
	str	x0, [sp, 24]
	mov	x0, 0
	mov	x0, x12
	bl	format_dec
	mov	x1, x0
	mov	x0, x12
	bl	out_str
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	ldr	x2, [sp, 24]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L320
	ldp	x29, x30, [sp, 32]
	add	sp, sp, 48
.LCFI40:
	ret
.L320:
.LCFI41:
	bl	__stack_chk_fail
.LFE57:
	.size	putint, .-putint
	.align	2
	.p2align 4,,11
	.global	putch
	.type	putch, %function
putch:
.LFB58:
	stp	x29, x30, [sp, -48]!
.LCFI42:
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI43:
	adrp	x19, .LANCHOR1
	add	x1, x19, :lo12:.LANCHOR1
	and	w20, w0, 255
	ldr	x2, [x1, 16]
	str	x21, [sp, 32]
.LCFI44:
	cmp	x2, 8192
	beq	.L322
	add	x1, x2, 1
	adrp	x21, out_buf
.L323:
	add	x0, x21, :lo12:out_buf
	add	x19, x19, :lo12:.LANCHOR1
	strb	w20, [x0, x2]
	ldr	x21, [sp, 32]
	str	x1, [x19, 16]
	ldp	x19, x20, [sp, 16]
	ldp	x29, x30, [sp], 48
.LCFI45:
	ret
	.p2align 2,,3
.L322:
.LCFI46:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x1, 1
	adrp	x21, out_buf
	add	x0, x21, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	mov	x1, 1
	mov	x2, 0
	b	.L323
.LFE58:
	.size	putch, .-putch
	.align	2
	.p2align 4,,11
	.global	putarray
	.type	putarray, %function
putarray:
.LFB59:
	sub	sp, sp, #112
.LCFI47:
	adrp	x2, :got:__stack_chk_guard
	ldr	x2, [x2, :got_lo12:__stack_chk_guard]
	stp	x29, x30, [sp, 32]
.LCFI48:
	add	x29, sp, 32
	stp	x19, x20, [sp, 48]
.LCFI49:
	add	x20, sp, 8
	mov	x19, x1
	stp	x21, x22, [sp, 64]
	mov	w1, w0
.LCFI50:
	mov	w22, w0
	stp	x25, x26, [sp, 96]
	ldr	x0, [x2]
	str	x0, [sp, 24]
	mov	x0, 0
	mov	x0, x20
.LCFI51:
	bl	format_dec
	mov	x1, x0
	mov	x0, x20
	bl	out_str
	mov	w0, 58
	bl	out_char
	cmp	w22, 0
	ble	.L338
	adrp	x25, .LANCHOR1
	adrp	x26, out_buf
	add	x22, x19, w22, sxtw 2
	add	x21, x25, :lo12:.LANCHOR1
	stp	x23, x24, [sp, 80]
.LCFI52:
	add	x24, x26, :lo12:out_buf
	mov	w23, 32
	b	.L331
	.p2align 2,,3
.L340:
	add	x1, x2, 1
.L330:
	strb	w23, [x24, x2]
	mov	x0, x20
	str	x1, [x21, 16]
	ldr	w1, [x19], 4
	bl	format_dec
	mov	x1, x0
	mov	x0, x20
	bl	out_str
	cmp	x19, x22
	beq	.L339
.L331:
	ldr	x2, [x21, 16]
	cmp	x2, 8192
	bne	.L340
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x1, 1
	add	x0, x26, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	mov	x1, 1
	mov	x2, 0
	b	.L330
	.p2align 2,,3
.L339:
	add	x0, x25, :lo12:.LANCHOR1
	ldp	x23, x24, [sp, 80]
.LCFI53:
	ldr	x2, [x0, 16]
	add	x3, x2, 1
	cmp	x2, 8192
	beq	.L341
.L333:
	add	x25, x25, :lo12:.LANCHOR1
	add	x26, x26, :lo12:out_buf
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	mov	w1, 10
	strb	w1, [x26, x2]
	str	x3, [x25, 16]
	ldr	x2, [sp, 24]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L342
	ldp	x29, x30, [sp, 32]
	ldp	x19, x20, [sp, 48]
	ldp	x21, x22, [sp, 64]
	ldp	x25, x26, [sp, 96]
	add	sp, sp, 112
.LCFI54:
	ret
	.p2align 2,,3
.L338:
.LCFI55:
	adrp	x25, .LANCHOR1
	add	x0, x25, :lo12:.LANCHOR1
	adrp	x26, out_buf
	ldr	x2, [x0, 16]
	add	x3, x2, 1
	cmp	x2, 8192
	bne	.L333
	.p2align 3,,7
.L341:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	add	x0, x26, :lo12:out_buf
	mov	x1, 1
	ldr	x3, [x3]
	bl	fwrite
	mov	x3, 1
	mov	x2, 0
	b	.L333
.L342:
	stp	x23, x24, [sp, 80]
.LCFI56:
	bl	__stack_chk_fail
.LFE59:
	.size	putarray, .-putarray
	.align	2
	.p2align 4,,11
	.global	putfloat
	.type	putfloat, %function
putfloat:
.LFB60:
	sub	sp, sp, #64
.LCFI57:
	adrp	x1, :got:__stack_chk_guard
	ldr	x1, [x1, :got_lo12:__stack_chk_guard]
	add	x10, sp, 8
	stp	x29, x30, [sp, 48]
.LCFI58:
	add	x29, sp, 48
	mov	x0, x10
	ldr	x2, [x1]
	str	x2, [sp, 40]
	mov	x2, 0
	bl	format_hex_float
	mov	x1, x0
	mov	x0, x10
	bl	out_str
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	ldr	x2, [sp, 40]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L346
	ldp	x29, x30, [sp, 48]
	add	sp, sp, 64
.LCFI59:
	ret
.L346:
.LCFI60:
	bl	__stack_chk_fail
.LFE60:
	.size	putfloat, .-putfloat
	.align	2
	.p2align 4,,11
	.global	putfarray
	.type	putfarray, %function
putfarray:
.LFB61:
	sub	sp, sp, #144
.LCFI61:
	adrp	x2, :got:__stack_chk_guard
	ldr	x2, [x2, :got_lo12:__stack_chk_guard]
	add	x12, sp, 8
	stp	x29, x30, [sp, 64]
.LCFI62:
	add	x29, sp, 64
	stp	x19, x20, [sp, 80]
.LCFI63:
	mov	x19, x1
	mov	w1, w0
	stp	x21, x22, [sp, 96]
.LCFI64:
	mov	w22, w0
	adrp	x20, .LANCHOR1
	stp	x23, x24, [sp, 112]
	ldr	x0, [x2]
	str	x0, [sp, 56]
	mov	x0, 0
	mov	x0, x12
.LCFI65:
	bl	format_dec
	mov	x1, x0
	mov	x0, x12
	bl	out_str
	add	x0, x20, :lo12:.LANCHOR1
	ldr	x0, [x0, 16]
	cmp	x0, 8192
	beq	.L348
	add	x2, x0, 1
	adrp	x23, out_buf
.L349:
	add	x20, x20, :lo12:.LANCHOR1
	add	x24, x23, :lo12:out_buf
	mov	w1, 58
	strb	w1, [x24, x0]
	str	x2, [x20, 16]
	cmp	w22, 0
	ble	.L350
	add	x21, sp, 24
	add	x22, x19, w22, sxtw 2
	str	x25, [sp, 128]
.LCFI66:
	mov	w25, 32
	b	.L353
	.p2align 2,,3
.L358:
	add	x1, x2, 1
.L352:
	strb	w25, [x24, x2]
	mov	x0, x21
	str	x1, [x20, 16]
	ldr	s0, [x19], 4
	bl	format_hex_float
	mov	x1, x0
	mov	x0, x21
	bl	out_str
	cmp	x19, x22
	beq	.L357
	ldr	x2, [x20, 16]
.L353:
	cmp	x2, 8192
	bne	.L358
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x1, 1
	add	x0, x23, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	mov	x1, 1
	mov	x2, 0
	b	.L352
	.p2align 2,,3
.L357:
	ldr	x25, [sp, 128]
.LCFI67:
.L350:
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	ldr	x2, [sp, 56]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L359
	ldp	x29, x30, [sp, 64]
	mov	w0, 10
	ldp	x19, x20, [sp, 80]
	ldp	x21, x22, [sp, 96]
	ldp	x23, x24, [sp, 112]
	add	sp, sp, 144
.LCFI68:
	b	out_char
	.p2align 2,,3
.L348:
.LCFI69:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	mov	x2, x0
	adrp	x23, out_buf
	mov	x1, 1
	add	x0, x23, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	mov	x2, 1
	mov	x0, 0
	b	.L349
.L359:
	str	x25, [sp, 128]
.LCFI70:
	bl	__stack_chk_fail
.LFE61:
	.size	putfarray, .-putfarray
	.align	2
	.p2align 4,,11
	.global	putf
	.type	putf, %function
putf:
.LFB62:
	sub	sp, sp, #352
.LCFI71:
	mov	w10, -56
	adrp	x8, :got:__stack_chk_guard
	ldr	x8, [x8, :got_lo12:__stack_chk_guard]
	add	x11, sp, 288
	stp	x29, x30, [sp, 112]
.LCFI72:
	add	x29, sp, 112
	mov	w9, -128
	stp	x19, x20, [sp, 128]
.LCFI73:
	adrp	x19, .LANCHOR1
	add	x19, x19, :lo12:.LANCHOR1
	str	q0, [sp, 160]
	mov	x20, x0
	str	q1, [sp, 176]
	str	q2, [sp, 192]
	str	q3, [sp, 208]
	str	q4, [sp, 224]
	str	q5, [sp, 240]
	str	q6, [sp, 256]
	str	q7, [sp, 272]
	stp	x1, x2, [sp, 296]
	stp	x3, x4, [sp, 312]
	stp	x5, x6, [sp, 328]
	str	x7, [sp, 344]
	str	x21, [sp, 144]
.LCFI74:
	ldr	x2, [x19, 16]
	ldr	x0, [x8]
	str	x0, [sp, 104]
	mov	x0, 0
	add	x0, sp, 352
	stp	x0, x0, [sp, 40]
	str	x11, [sp, 56]
	stp	w10, w9, [sp, 64]
	adrp	x21, :got:stdout
	ldr	x21, [x21, :got_lo12:stdout]
	cbnz	x2, .L365
.L361:
	add	x0, sp, 40
	add	x4, sp, 72
	mov	x3, sp
	mov	x2, x20
	mov	w1, 2
	ldp	q0, q1, [x0]
	ldr	x0, [x21]
	stp	q0, q1, [x4]
	stp	q0, q1, [x3]
	bl	__vfprintf_chk
	adrp	x0, :got:__stack_chk_guard
	ldr	x0, [x0, :got_lo12:__stack_chk_guard]
	ldr	x2, [sp, 104]
	ldr	x1, [x0]
	subs	x2, x2, x1
	mov	x1, 0
	bne	.L367
	ldp	x29, x30, [sp, 112]
	ldp	x19, x20, [sp, 128]
	ldr	x21, [sp, 144]
	add	sp, sp, 352
.LCFI75:
	ret
	.p2align 2,,3
.L365:
.LCFI76:
	ldr	x3, [x21]
	adrp	x0, out_buf
	mov	x1, 1
	add	x0, x0, :lo12:out_buf
	bl	fwrite
	str	xzr, [x19, 16]
	b	.L361
.L367:
	bl	__stack_chk_fail
.LFE62:
	.size	putf, .-putf
	.section	.text.startup,"ax",@progbits
	.align	2
	.p2align 4,,11
	.global	before_main
	.type	before_main, %function
before_main:
.LFB63:
	movi	v0.4s, 0
	adrp	x4, _sysy_us
	adrp	x3, _sysy_s
	adrp	x2, _sysy_m
	adrp	x1, _sysy_h
	add	x4, x4, :lo12:_sysy_us
	add	x3, x3, :lo12:_sysy_s
	add	x2, x2, :lo12:_sysy_m
	add	x1, x1, :lo12:_sysy_h
	mov	x0, 0
	.p2align 3,,7
.L369:
	str	q0, [x4, x0]
	str	q0, [x3, x0]
	str	q0, [x2, x0]
	str	q0, [x1, x0]
	add	x0, x0, 16
	cmp	x0, 4096
	bne	.L369
	adrp	x0, .LANCHOR1+24
	mov	w1, 1
	str	w1, [x0, #:lo12:.LANCHOR1+24]
	ret
.LFE63:
	.size	before_main, .-before_main
	.section	.init_array,"aw"
	.align	3
	.xword	before_main
	.section	.rodata.str1.8,"aMS",@progbits,1
	.align	3
.LC0:
	.string	"Timer@%04d-%04d: %dH-%dM-%dS-%dus\n"
	.align	3
.LC1:
	.string	"TOTAL: %dH-%dM-%dS-%dus\n"
	.section	.text.exit,"ax",@progbits
	.align	2
	.p2align 4,,11
	.global	after_main
	.type	after_main, %function
after_main:
.LFB64:
	sub	sp, sp, #112
.LCFI77:
	stp	x29, x30, [sp, 16]
.LCFI78:
	add	x29, sp, 16
	stp	x19, x20, [sp, 32]
.LCFI79:
	adrp	x19, .LANCHOR1
	add	x20, x19, :lo12:.LANCHOR1
	ldr	x2, [x20, 16]
	cbnz	x2, .L381
.L372:
	add	x0, x19, :lo12:.LANCHOR1
	ldr	w0, [x0, 24]
	cmp	w0, 1
	ble	.L373
	adrp	x20, :got:stderr
	ldr	x20, [x20, :got_lo12:stderr]
	stp	x21, x22, [sp, 48]
.LCFI80:
	mov	w21, 34953
	stp	x27, x28, [sp, 96]
.LCFI81:
	adrp	x28, _sysy_s
	adrp	x27, _sysy_m
	add	x28, x28, :lo12:_sysy_s
	add	x27, x27, :lo12:_sysy_m
	adrp	x22, _sysy_us
	movk	w21, 0x8888, lsl 16
	stp	x23, x24, [sp, 64]
.LCFI82:
	adrp	x23, _sysy_h
	stp	x25, x26, [sp, 80]
.LCFI83:
	mov	x26, 1
	.p2align 3,,7
.L374:
	adrp	x0, _sysy_l2
	add	x11, x23, :lo12:_sysy_h
	add	x3, x0, :lo12:_sysy_l2
	add	x25, x22, :lo12:_sysy_us
	adrp	x0, _sysy_l1
	add	x0, x0, :lo12:_sysy_l1
	ldr	w7, [x28, x26, lsl 2]
	ldr	w5, [x11, x26, lsl 2]
	adrp	x1, .LC0
	ldr	w4, [x3, x26, lsl 2]
	add	x2, x1, :lo12:.LC0
	ldr	w3, [x0, x26, lsl 2]
	mov	x24, x11
	ldr	x0, [x20]
	mov	w1, 2
	ldr	w6, [x27, x26, lsl 2]
	ldr	w13, [x25, x26, lsl 2]
	str	w13, [sp]
	bl	__fprintf_chk
	ldr	w3, [x28, x26, lsl 2]
	add	x2, x19, :lo12:.LANCHOR1
	ldr	w1, [x28]
	mov	w8, 16960
	ldr	w4, [x27, x26, lsl 2]
	movk	w8, 0xf, lsl 16
	ldr	w0, [x27]
	add	w1, w1, w3
	ldr	w7, [x22, #:lo12:_sysy_us]
	ldr	w3, [x25, x26, lsl 2]
	add	w0, w0, w4
	ldr	w6, [x24, x26, lsl 2]
	smull	x5, w1, w21
	add	w7, w7, w3
	ldr	w3, [x23, #:lo12:_sysy_h]
	smull	x4, w0, w21
	ldr	w2, [x2, 24]
	add	w3, w3, w6
	mov	w6, 56963
	lsr	x5, x5, 32
	movk	w6, 0x431b, lsl 16
	lsr	x4, x4, 32
	add	w5, w1, w5
	smull	x6, w7, w6
	add	w4, w0, w4
	asr	w5, w5, 5
	add	x26, x26, 1
	sub	w5, w5, w1, asr 31
	asr	w4, w4, 5
	sub	w4, w4, w0, asr 31
	asr	x6, x6, 50
	lsl	w11, w5, 4
	sub	w6, w6, w7, asr 31
	sub	w5, w11, w5
	lsl	w11, w4, 4
	sub	w4, w11, w4
	str	w3, [x23, #:lo12:_sysy_h]
	msub	w6, w6, w8, w7
	sub	w5, w1, w5, lsl 2
	sub	w4, w0, w4, lsl 2
	str	w6, [x22, #:lo12:_sysy_us]
	str	w4, [x27]
	str	w5, [x28]
	cmp	w2, w26
	bgt	.L374
	ldp	x21, x22, [sp, 48]
.LCFI84:
	ldp	x23, x24, [sp, 64]
.LCFI85:
	ldp	x25, x26, [sp, 80]
.LCFI86:
	ldp	x27, x28, [sp, 96]
.LCFI87:
.L375:
	adrp	x2, .LC1
	ldr	x0, [x20]
	add	x2, x2, :lo12:.LC1
	ldp	x29, x30, [sp, 16]
	mov	w1, 2
	ldp	x19, x20, [sp, 32]
	add	sp, sp, 112
.LCFI88:
	b	__fprintf_chk
.L381:
.LCFI89:
	adrp	x3, :got:stdout
	ldr	x3, [x3, :got_lo12:stdout]
	adrp	x0, out_buf
	mov	x1, 1
	add	x0, x0, :lo12:out_buf
	ldr	x3, [x3]
	bl	fwrite
	str	xzr, [x20, 16]
	b	.L372
.L373:
	adrp	x3, _sysy_h
	adrp	x2, _sysy_m
	adrp	x1, _sysy_s
	adrp	x0, _sysy_us
	ldr	w3, [x3, #:lo12:_sysy_h]
	ldr	w4, [x2, #:lo12:_sysy_m]
	ldr	w5, [x1, #:lo12:_sysy_s]
	ldr	w6, [x0, #:lo12:_sysy_us]
	adrp	x20, :got:stderr
	ldr	x20, [x20, :got_lo12:stderr]
	b	.L375
.LFE64:
	.size	after_main, .-after_main
	.section	.fini_array,"aw"
	.align	3
	.xword	after_main
	.text
	.align	2
	.p2align 4,,11
	.global	_sysy_starttime
	.type	_sysy_starttime, %function
_sysy_starttime:
.LFB65:
	adrp	x2, .LANCHOR1
	add	x2, x2, :lo12:.LANCHOR1
	adrp	x3, _sysy_l1
	add	x3, x3, :lo12:_sysy_l1
	mov	w5, w0
	mov	x1, 0
	ldrsw	x4, [x2, 24]
	add	x0, x2, 32
	str	w5, [x3, x4, lsl 2]
	b	gettimeofday
.LFE65:
	.size	_sysy_starttime, .-_sysy_starttime
	.align	2
	.p2align 4,,11
	.global	_sysy_stoptime
	.type	_sysy_stoptime, %function
_sysy_stoptime:
.LFB66:
	stp	x29, x30, [sp, -32]!
.LCFI90:
	mov	x1, 0
	mov	x29, sp
	stp	x19, x20, [sp, 16]
.LCFI91:
	adrp	x19, .LANCHOR1
	add	x19, x19, :lo12:.LANCHOR1
	mov	w20, w0
	add	x0, x19, 48
	bl	gettimeofday
	ldp	x0, x1, [x19, 32]
	adrp	x10, _sysy_us
	ldp	x4, x5, [x19, 48]
	add	x10, x10, :lo12:_sysy_us
	ldr	w12, [x19, 24]
	mov	w14, 16960
	movk	w14, 0xf, lsl 16
	mov	w2, 56963
	movk	w2, 0x431b, lsl 16
	adrp	x9, _sysy_s
	sxtw	x3, w12
	add	x9, x9, :lo12:_sysy_s
	sub	x4, x4, x0
	adrp	x7, _sysy_m
	add	x7, x7, :lo12:_sysy_m
	adrp	x8, _sysy_h
	ldr	w0, [x10, x3, lsl 2]
	add	x8, x8, :lo12:_sysy_h
	madd	w4, w14, w4, w5
	ldr	w6, [x9, x3, lsl 2]
	sub	w0, w0, w1
	mov	w1, 34953
	add	w4, w4, w0
	movk	w1, 0x8888, lsl 16
	ldr	w5, [x7, x3, lsl 2]
	adrp	x13, _sysy_l2
	ldr	w11, [x8, x3, lsl 2]
	add	x13, x13, :lo12:_sysy_l2
	smull	x2, w4, w2
	add	w12, w12, 1
	str	w12, [x19, 24]
	str	w20, [x13, x3, lsl 2]
	asr	x2, x2, 50
	sub	w2, w2, w4, asr 31
	add	w6, w2, w6
	ldp	x19, x20, [sp, 16]
	msub	w2, w2, w14, w4
	smull	x0, w6, w1
	str	w2, [x10, x3, lsl 2]
	lsr	x0, x0, 32
	add	w0, w6, w0
	asr	w0, w0, 5
	sub	w0, w0, w6, asr 31
	add	w2, w0, w5
	lsl	w4, w0, 4
	sub	w4, w4, w0
	smull	x0, w2, w1
	sub	w1, w6, w4, lsl 2
	str	w1, [x9, x3, lsl 2]
	lsr	x0, x0, 32
	add	w0, w2, w0
	asr	w0, w0, 5
	sub	w0, w0, w2, asr 31
	add	w11, w11, w0
	str	w11, [x8, x3, lsl 2]
	lsl	w1, w0, 4
	sub	w0, w1, w0
	sub	w0, w2, w0, lsl 2
	str	w0, [x7, x3, lsl 2]
	ldp	x29, x30, [sp], 32
.LCFI92:
	ret
.LFE66:
	.size	_sysy_stoptime, .-_sysy_stoptime
	.global	_sysy_idx
	.global	_sysy_us
	.global	_sysy_s
	.global	_sysy_m
	.global	_sysy_h
	.global	_sysy_l2
	.global	_sysy_l1
	.global	_sysy_end
	.global	_sysy_start
	.section	.rodata
	.align	4
	.set	.LANCHOR2,. + 0
	.type	hex_digits, %object
	.size	hex_digits, 17
hex_digits:
	.string	"0123456789abcdef"
	.zero	15
	.type	dec2_tbl, %object
	.size	dec2_tbl, 201
dec2_tbl:
	.string	"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899"
	.data
	.align	2
	.set	.LANCHOR0,. + 0
	.type	in_unget, %object
	.size	in_unget, 4
in_unget:
	.word	-1
	.bss
	.align	4
	.set	.LANCHOR1,. + 0
	.type	in_pos, %object
	.size	in_pos, 8
in_pos:
	.zero	8
	.type	in_len, %object
	.size	in_len, 8
in_len:
	.zero	8
	.type	out_len, %object
	.size	out_len, 8
out_len:
	.zero	8
	.type	_sysy_idx, %object
	.size	_sysy_idx, 4
_sysy_idx:
	.zero	4
	.zero	4
	.type	_sysy_start, %object
	.size	_sysy_start, 16
_sysy_start:
	.zero	16
	.type	_sysy_end, %object
	.size	_sysy_end, 16
_sysy_end:
	.zero	16
	.type	in_buf, %object
	.size	in_buf, 8192
in_buf:
	.zero	8192
	.type	out_buf, %object
	.size	out_buf, 8192
out_buf:
	.zero	8192
	.type	_sysy_us, %object
	.size	_sysy_us, 4096
_sysy_us:
	.zero	4096
	.type	_sysy_s, %object
	.size	_sysy_s, 4096
_sysy_s:
	.zero	4096
	.type	_sysy_m, %object
	.size	_sysy_m, 4096
_sysy_m:
	.zero	4096
	.type	_sysy_h, %object
	.size	_sysy_h, 4096
_sysy_h:
	.zero	4096
	.type	_sysy_l2, %object
	.size	_sysy_l2, 4096
_sysy_l2:
	.zero	4096
	.type	_sysy_l1, %object
	.size	_sysy_l1, 4096
_sysy_l1:
	.zero	4096
	.section	.eh_frame,"a",@progbits
.Lframe1:
	.4byte	.LECIE1-.LSCIE1
.LSCIE1:
	.4byte	0
	.byte	0x3
	.string	"zR"
	.uleb128 0x1
	.sleb128 -8
	.uleb128 0x1e
	.uleb128 0x1
	.byte	0x1b
	.byte	0xc
	.uleb128 0x1f
	.uleb128 0
	.align	3
.LECIE1:
.LSFDE1:
	.4byte	.LEFDE1-.LASFDE1
.LASFDE1:
	.4byte	.LASFDE1-.Lframe1
	.4byte	.LFB47-.
	.4byte	.LFE47-.LFB47
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI0-.LFB47
	.byte	0xe
	.uleb128 0x20
	.byte	0x9d
	.uleb128 0x4
	.byte	0x9e
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI1-.LCFI0
	.byte	0x93
	.uleb128 0x2
	.byte	0x94
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI2-.LCFI1
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI3-.LCFI2
	.byte	0xb
	.align	3
.LEFDE1:
.LSFDE3:
	.4byte	.LEFDE3-.LASFDE3
.LASFDE3:
	.4byte	.LASFDE3-.Lframe1
	.4byte	.LFB45-.
	.4byte	.LFE45-.LFB45
	.uleb128 0
	.align	3
.LEFDE3:
.LSFDE5:
	.4byte	.LEFDE5-.LASFDE5
.LASFDE5:
	.4byte	.LASFDE5-.Lframe1
	.4byte	.LFB43-.
	.4byte	.LFE43-.LFB43
	.uleb128 0
	.align	3
.LEFDE5:
.LSFDE7:
	.4byte	.LEFDE7-.LASFDE7
.LASFDE7:
	.4byte	.LASFDE7-.Lframe1
	.4byte	.LFB41-.
	.4byte	.LFE41-.LFB41
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI4-.LFB41
	.byte	0xe
	.uleb128 0x50
	.byte	0x9d
	.uleb128 0xa
	.byte	0x9e
	.uleb128 0x9
	.byte	0x4
	.4byte	.LCFI5-.LCFI4
	.byte	0x95
	.uleb128 0x6
	.byte	0x96
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI6-.LCFI5
	.byte	0x93
	.uleb128 0x8
	.byte	0x94
	.uleb128 0x7
	.byte	0x4
	.4byte	.LCFI7-.LCFI6
	.byte	0x97
	.uleb128 0x4
	.byte	0x98
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI8-.LCFI7
	.byte	0x99
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI9-.LCFI8
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd9
	.byte	0xd7
	.byte	0xd8
	.byte	0xd5
	.byte	0xd6
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI10-.LCFI9
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI11-.LCFI10
	.byte	0xe
	.uleb128 0
	.byte	0xd3
	.byte	0xd4
	.byte	0xd5
	.byte	0xd6
	.byte	0xd7
	.byte	0xd8
	.byte	0xd9
	.byte	0xdd
	.byte	0xde
	.align	3
.LEFDE7:
.LSFDE9:
	.4byte	.LEFDE9-.LASFDE9
.LASFDE9:
	.4byte	.LASFDE9-.Lframe1
	.4byte	.LFB40-.
	.4byte	.LFE40-.LFB40
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI12-.LFB40
	.byte	0xe
	.uleb128 0x30
	.byte	0x9d
	.uleb128 0x6
	.byte	0x9e
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI13-.LCFI12
	.byte	0x93
	.uleb128 0x4
	.byte	0x94
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI14-.LCFI13
	.byte	0x95
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI15-.LCFI14
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd5
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI16-.LCFI15
	.byte	0xb
	.align	3
.LEFDE9:
.LSFDE11:
	.4byte	.LEFDE11-.LASFDE11
.LASFDE11:
	.4byte	.LASFDE11-.Lframe1
	.4byte	.LFB50-.
	.4byte	.LFE50-.LFB50
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI17-.LFB50
	.byte	0xe
	.uleb128 0x20
	.byte	0x9d
	.uleb128 0x4
	.byte	0x9e
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI18-.LCFI17
	.byte	0x93
	.uleb128 0x2
	.byte	0x94
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI19-.LCFI18
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI20-.LCFI19
	.byte	0xb
	.align	3
.LEFDE11:
.LSFDE13:
	.4byte	.LEFDE13-.LASFDE13
.LASFDE13:
	.4byte	.LASFDE13-.Lframe1
	.4byte	.LFB51-.
	.4byte	.LFE51-.LFB51
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI21-.LFB51
	.byte	0xe
	.uleb128 0x20
	.byte	0x9d
	.uleb128 0x4
	.byte	0x9e
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI22-.LCFI21
	.byte	0x93
	.uleb128 0x2
	.byte	0x94
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI23-.LCFI22
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI24-.LCFI23
	.byte	0xb
	.align	3
.LEFDE13:
.LSFDE15:
	.4byte	.LEFDE15-.LASFDE15
.LASFDE15:
	.4byte	.LASFDE15-.Lframe1
	.4byte	.LFB54-.
	.4byte	.LFE54-.LFB54
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI25-.LFB54
	.byte	0xe
	.uleb128 0x70
	.byte	0x4
	.4byte	.LCFI26-.LCFI25
	.byte	0x9d
	.uleb128 0x4
	.byte	0x9e
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI27-.LCFI26
	.byte	0x93
	.uleb128 0x2
	.byte	0x94
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI28-.LCFI27
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI29-.LCFI28
	.byte	0xb
	.align	3
.LEFDE15:
.LSFDE17:
	.4byte	.LEFDE17-.LASFDE17
.LASFDE17:
	.4byte	.LASFDE17-.Lframe1
	.4byte	.LFB55-.
	.4byte	.LFE55-.LFB55
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI30-.LFB55
	.byte	0xe
	.uleb128 0x30
	.byte	0x9d
	.uleb128 0x6
	.byte	0x9e
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI31-.LCFI30
	.byte	0x93
	.uleb128 0x4
	.byte	0x94
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI32-.LCFI31
	.byte	0x95
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI33-.LCFI32
	.byte	0xde
	.byte	0xdd
	.byte	0xd5
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.align	3
.LEFDE17:
.LSFDE19:
	.4byte	.LEFDE19-.LASFDE19
.LASFDE19:
	.4byte	.LASFDE19-.Lframe1
	.4byte	.LFB56-.
	.4byte	.LFE56-.LFB56
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI34-.LFB56
	.byte	0xe
	.uleb128 0x30
	.byte	0x9d
	.uleb128 0x6
	.byte	0x9e
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI35-.LCFI34
	.byte	0x93
	.uleb128 0x4
	.byte	0x94
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI36-.LCFI35
	.byte	0x95
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI37-.LCFI36
	.byte	0xde
	.byte	0xdd
	.byte	0xd5
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.align	3
.LEFDE19:
.LSFDE21:
	.4byte	.LEFDE21-.LASFDE21
.LASFDE21:
	.4byte	.LASFDE21-.Lframe1
	.4byte	.LFB57-.
	.4byte	.LFE57-.LFB57
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI38-.LFB57
	.byte	0xe
	.uleb128 0x30
	.byte	0x4
	.4byte	.LCFI39-.LCFI38
	.byte	0x9d
	.uleb128 0x2
	.byte	0x9e
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI40-.LCFI39
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI41-.LCFI40
	.byte	0xb
	.align	3
.LEFDE21:
.LSFDE23:
	.4byte	.LEFDE23-.LASFDE23
.LASFDE23:
	.4byte	.LASFDE23-.Lframe1
	.4byte	.LFB58-.
	.4byte	.LFE58-.LFB58
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI42-.LFB58
	.byte	0xe
	.uleb128 0x30
	.byte	0x9d
	.uleb128 0x6
	.byte	0x9e
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI43-.LCFI42
	.byte	0x93
	.uleb128 0x4
	.byte	0x94
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI44-.LCFI43
	.byte	0x95
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI45-.LCFI44
	.byte	0xa
	.byte	0xde
	.byte	0xdd
	.byte	0xd5
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI46-.LCFI45
	.byte	0xb
	.align	3
.LEFDE23:
.LSFDE25:
	.4byte	.LEFDE25-.LASFDE25
.LASFDE25:
	.4byte	.LASFDE25-.Lframe1
	.4byte	.LFB59-.
	.4byte	.LFE59-.LFB59
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI47-.LFB59
	.byte	0xe
	.uleb128 0x70
	.byte	0x4
	.4byte	.LCFI48-.LCFI47
	.byte	0x9d
	.uleb128 0xa
	.byte	0x9e
	.uleb128 0x9
	.byte	0x4
	.4byte	.LCFI49-.LCFI48
	.byte	0x93
	.uleb128 0x8
	.byte	0x94
	.uleb128 0x7
	.byte	0x4
	.4byte	.LCFI50-.LCFI49
	.byte	0x95
	.uleb128 0x6
	.byte	0x96
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI51-.LCFI50
	.byte	0x99
	.uleb128 0x2
	.byte	0x9a
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI52-.LCFI51
	.byte	0x98
	.uleb128 0x3
	.byte	0x97
	.uleb128 0x4
	.byte	0x4
	.4byte	.LCFI53-.LCFI52
	.byte	0xd8
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI54-.LCFI53
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xd9
	.byte	0xda
	.byte	0xd5
	.byte	0xd6
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI55-.LCFI54
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI56-.LCFI55
	.byte	0x98
	.uleb128 0x3
	.byte	0x97
	.uleb128 0x4
	.align	3
.LEFDE25:
.LSFDE27:
	.4byte	.LEFDE27-.LASFDE27
.LASFDE27:
	.4byte	.LASFDE27-.Lframe1
	.4byte	.LFB60-.
	.4byte	.LFE60-.LFB60
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI57-.LFB60
	.byte	0xe
	.uleb128 0x40
	.byte	0x4
	.4byte	.LCFI58-.LCFI57
	.byte	0x9d
	.uleb128 0x2
	.byte	0x9e
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI59-.LCFI58
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI60-.LCFI59
	.byte	0xb
	.align	3
.LEFDE27:
.LSFDE29:
	.4byte	.LEFDE29-.LASFDE29
.LASFDE29:
	.4byte	.LASFDE29-.Lframe1
	.4byte	.LFB61-.
	.4byte	.LFE61-.LFB61
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI61-.LFB61
	.byte	0xe
	.uleb128 0x90
	.byte	0x4
	.4byte	.LCFI62-.LCFI61
	.byte	0x9d
	.uleb128 0xa
	.byte	0x9e
	.uleb128 0x9
	.byte	0x4
	.4byte	.LCFI63-.LCFI62
	.byte	0x93
	.uleb128 0x8
	.byte	0x94
	.uleb128 0x7
	.byte	0x4
	.4byte	.LCFI64-.LCFI63
	.byte	0x95
	.uleb128 0x6
	.byte	0x96
	.uleb128 0x5
	.byte	0x4
	.4byte	.LCFI65-.LCFI64
	.byte	0x97
	.uleb128 0x4
	.byte	0x98
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI66-.LCFI65
	.byte	0x99
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI67-.LCFI66
	.byte	0xd9
	.byte	0x4
	.4byte	.LCFI68-.LCFI67
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xd7
	.byte	0xd8
	.byte	0xd5
	.byte	0xd6
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI69-.LCFI68
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI70-.LCFI69
	.byte	0x99
	.uleb128 0x2
	.align	3
.LEFDE29:
.LSFDE31:
	.4byte	.LEFDE31-.LASFDE31
.LASFDE31:
	.4byte	.LASFDE31-.Lframe1
	.4byte	.LFB62-.
	.4byte	.LFE62-.LFB62
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI71-.LFB62
	.byte	0xe
	.uleb128 0x160
	.byte	0x4
	.4byte	.LCFI72-.LCFI71
	.byte	0x9d
	.uleb128 0x1e
	.byte	0x9e
	.uleb128 0x1d
	.byte	0x4
	.4byte	.LCFI73-.LCFI72
	.byte	0x93
	.uleb128 0x1c
	.byte	0x94
	.uleb128 0x1b
	.byte	0x4
	.4byte	.LCFI74-.LCFI73
	.byte	0x95
	.uleb128 0x1a
	.byte	0x4
	.4byte	.LCFI75-.LCFI74
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xd5
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI76-.LCFI75
	.byte	0xb
	.align	3
.LEFDE31:
.LSFDE33:
	.4byte	.LEFDE33-.LASFDE33
.LASFDE33:
	.4byte	.LASFDE33-.Lframe1
	.4byte	.LFB63-.
	.4byte	.LFE63-.LFB63
	.uleb128 0
	.align	3
.LEFDE33:
.LSFDE35:
	.4byte	.LEFDE35-.LASFDE35
.LASFDE35:
	.4byte	.LASFDE35-.Lframe1
	.4byte	.LFB64-.
	.4byte	.LFE64-.LFB64
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI77-.LFB64
	.byte	0xe
	.uleb128 0x70
	.byte	0x4
	.4byte	.LCFI78-.LCFI77
	.byte	0x9d
	.uleb128 0xc
	.byte	0x9e
	.uleb128 0xb
	.byte	0x4
	.4byte	.LCFI79-.LCFI78
	.byte	0x93
	.uleb128 0xa
	.byte	0x94
	.uleb128 0x9
	.byte	0x4
	.4byte	.LCFI80-.LCFI79
	.byte	0x96
	.uleb128 0x7
	.byte	0x95
	.uleb128 0x8
	.byte	0x4
	.4byte	.LCFI81-.LCFI80
	.byte	0x9c
	.uleb128 0x1
	.byte	0x9b
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI82-.LCFI81
	.byte	0x98
	.uleb128 0x5
	.byte	0x97
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI83-.LCFI82
	.byte	0x9a
	.uleb128 0x3
	.byte	0x99
	.uleb128 0x4
	.byte	0x4
	.4byte	.LCFI84-.LCFI83
	.byte	0xd6
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI85-.LCFI84
	.byte	0xd8
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI86-.LCFI85
	.byte	0xda
	.byte	0xd9
	.byte	0x4
	.4byte	.LCFI87-.LCFI86
	.byte	0xdc
	.byte	0xdb
	.byte	0x4
	.4byte	.LCFI88-.LCFI87
	.byte	0xa
	.byte	0xdd
	.byte	0xde
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI89-.LCFI88
	.byte	0xb
	.align	3
.LEFDE35:
.LSFDE37:
	.4byte	.LEFDE37-.LASFDE37
.LASFDE37:
	.4byte	.LASFDE37-.Lframe1
	.4byte	.LFB65-.
	.4byte	.LFE65-.LFB65
	.uleb128 0
	.align	3
.LEFDE37:
.LSFDE39:
	.4byte	.LEFDE39-.LASFDE39
.LASFDE39:
	.4byte	.LASFDE39-.Lframe1
	.4byte	.LFB66-.
	.4byte	.LFE66-.LFB66
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI90-.LFB66
	.byte	0xe
	.uleb128 0x20
	.byte	0x9d
	.uleb128 0x4
	.byte	0x9e
	.uleb128 0x3
	.byte	0x4
	.4byte	.LCFI91-.LCFI90
	.byte	0x93
	.uleb128 0x2
	.byte	0x94
	.uleb128 0x1
	.byte	0x4
	.4byte	.LCFI92-.LCFI91
	.byte	0xde
	.byte	0xdd
	.byte	0xd3
	.byte	0xd4
	.byte	0xe
	.uleb128 0
	.align	3
.LEFDE39:
	.hidden	strtof
	.ident	"GCC: (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0"
	.section	.note.GNU-stack,"",@progbits
