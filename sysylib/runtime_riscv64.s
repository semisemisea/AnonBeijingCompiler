	.file	"sylib.c"
	.option pic
	.attribute arch, "rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zifencei2p0"
	.attribute unaligned_access, 0
	.attribute stack_align, 16
	.text
	.align	1
	.type	in_get, @function
in_get:
.LFB47:
	lla	a5,.LANCHOR0
	lw	a0,0(a5)
	blt	a0,zero,.L2
	li	a4,-1
	sw	a4,0(a5)
	ret
.L2:
	addi	sp,sp,-16
.LCFI0:
	sd	s0,0(sp)
.LCFI1:
	lla	s0,.LANCHOR1
	ld	a5,0(s0)
	ld	a0,8(s0)
	sd	ra,8(sp)
.LCFI2:
	beq	a5,a0,.L10
.L4:
	bleu	a0,a5,.L5
	lla	a4,in_buf
	addi	a3,a5,1
	add	a5,a4,a5
	lbu	a0,0(a5)
	sd	a3,0(s0)
.L3:
	ld	ra,8(sp)
.LCFI3:
	ld	s0,0(sp)
.LCFI4:
	addi	sp,sp,16
.LCFI5:
	jr	ra
.L10:
.LCFI6:
	la	a5,stdin
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,in_buf
	call	fread@plt
	sd	zero,.LANCHOR1,a5
	sd	a0,8(s0)
	li	a5,0
	j	.L4
.L5:
	li	a0,-1
	j	.L3
.LFE47:
	.size	in_get, .-in_get
	.align	1
	.type	format_hex_float, @function
format_hex_float:
.LFB45:
	fmv.x.s	a4,fa0
	li	a3,255
	srliw	a2,a4,23
	slli	a5,a4,41
	andi	a2,a2,0xff
	srliw	a4,a4,31
	srli	a5,a5,41
	beq	a2,a3,.L47
	mv	a6,a0
	beq	a4,zero,.L16
	li	a4,45
	addi	a6,a0,1
	sb	a4,0(a0)
.L16:
	li	a3,48
	li	a4,120
	sb	a3,0(a6)
	sb	a4,1(a6)
	bne	a2,zero,.L17
	li	a4,23
	beq	a5,zero,.L48
.L18:
	addiw	a1,a4,-1
	mv	a2,a4
	sext.w	a4,a1
	srlw	a3,a5,a4
	andi	a3,a3,1
	beq	a4,zero,.L34
	beq	a3,zero,.L18
	li	a4,23
	subw	a4,a4,a1
	addiw	a2,a2,-150
.L19:
	sllw	a5,a5,a4
	slli	a5,a5,41
	srli	a5,a5,41
	j	.L20
.L17:
	addiw	a2,a2,-127
.L20:
	li	a4,49
	addi	a7,a6,3
	sb	a4,2(a6)
	slliw	a3,a5,1
	mv	a1,a7
	li	a4,20
	lla	t3,.LANCHOR2
	li	t1,-4
.L21:
	srlw	a5,a3,a4
	andi	a5,a5,15
	add	a5,t3,a5
	lbu	a5,0(a5)
	addiw	a4,a4,-4
	addi	a1,a1,1
	sb	a5,-1(a1)
	bne	a4,t1,.L21
	addi	a4,a6,9
	li	a5,6
	li	a1,48
	j	.L22
.L24:
	addiw	a5,a5,-1
	beq	a5,zero,.L23
.L22:
	lbu	a3,-1(a4)
	addi	a4,a4,-1
	beq	a3,a1,.L24
	addi	a4,a5,2
	add	a4,a6,a4
	sext.w	a5,a5
	addi	a1,a6,2
.L26:
	lbu	a3,0(a4)
	addi	a4,a4,-1
	sb	a3,2(a4)
	bne	a4,a1,.L26
	addi	a5,a5,4
	li	a4,46
	sb	a4,3(a6)
	add	a7,a6,a5
.L23:
	li	a5,112
	sb	a5,0(a7)
	li	a5,43
	bge	a2,zero,.L27
	negw	a2,a2
	li	a5,45
.L27:
	sb	a5,1(a7)
	li	a5,99
	ble	a2,a5,.L28
	li	a5,100
	remw	a5,a2,a5
	li	a2,10
	li	a4,49
	sb	a4,2(a7)
	addi	a4,a7,5
	sub	a0,a4,a0
	divw	a3,a5,a2
	remw	a5,a5,a2
	addiw	a3,a3,48
	sb	a3,3(a7)
	addiw	a5,a5,48
	sb	a5,4(a7)
	ret
.L47:
	bne	a5,zero,.L49
	mv	a5,a0
	beq	a4,zero,.L15
	li	a4,45
	addi	a5,a0,1
	sb	a4,0(a0)
.L15:
	li	a3,105
	sb	a3,0(a5)
	li	a3,110
	addi	a4,a5,3
	sb	a3,1(a5)
	li	a3,102
	sb	a3,2(a5)
	sub	a0,a4,a0
	ret
.L28:
	li	a5,9
	ble	a2,a5,.L30
	li	a5,10
	divw	a3,a2,a5
	addi	a4,a7,4
	sub	a0,a4,a0
	remw	a5,a2,a5
	addiw	a3,a3,48
	sb	a3,2(a7)
	addiw	a5,a5,48
	sb	a5,3(a7)
	ret
.L30:
	addiw	a2,a2,48
	addi	a4,a7,3
	sb	a2,2(a7)
	sub	a0,a4,a0
	ret
.L49:
	li	a5,110
	li	a4,97
	sb	a5,0(a0)
	sb	a4,1(a0)
	sb	a5,2(a0)
	li	a0,3
	ret
.L48:
	li	a4,112
	addi	a5,a6,6
	sb	a4,3(a6)
	li	a4,43
	sb	a3,2(a6)
	sb	a4,4(a6)
	sb	a3,5(a6)
	sub	a0,a5,a0
	ret
.L34:
	li	a2,-149
	li	a4,23
	j	.L19
.LFE45:
	.size	format_hex_float, .-format_hex_float
	.align	1
	.type	format_dec, @function
format_dec:
.LFB43:
	mv	a6,a0
	sext.w	a3,a1
	bge	a1,zero,.L51
	negw	a3,a3
.L51:
	li	a5,99
	addi	a0,a6,12
	bleu	a3,a5,.L61
	li	t4,8192
	mv	a2,a0
	lla	t1,.LANCHOR2
	li	a7,100
	addi	t4,t4,1807
.L53:
	remuw	a5,a3,a7
	addi	a2,a2,-2
	sext.w	t3,a3
	slliw	a5,a5,1
	addiw	a4,a5,1
	slli	a4,a4,32
	slli	a5,a5,32
	srli	a4,a4,32
	srli	a5,a5,32
	add	a4,t1,a4
	add	a5,t1,a5
	lbu	a4,24(a4)
	lbu	a5,24(a5)
	divuw	a3,a3,a7
	sb	a4,1(a2)
	sb	a5,0(a2)
	bgtu	t3,t4,.L53
.L52:
	li	a5,9
	bleu	a3,a5,.L54
	slliw	a3,a3,1
	slli	a4,a3,32
	lla	a5,.LANCHOR2
	srli	a4,a4,32
	add	a4,a5,a4
	addiw	a3,a3,1
	lbu	a4,24(a4)
	slli	a3,a3,32
	srli	a3,a3,32
	add	a5,a5,a3
	lbu	a3,24(a5)
	sb	a4,-2(a2)
	addi	a5,a2,-2
.L55:
	sb	a3,-1(a2)
	bge	a1,zero,.L56
	li	a4,45
	sb	a4,-1(a5)
	addi	a5,a5,-1
.L56:
	sub	a0,a0,a5
	bgeu	a5,a6,.L57
	mv	a4,a0
	beq	a0,zero,.L70
.L58:
	addi	a4,a4,-1
	add	a3,a5,a4
	lbu	a2,0(a3)
	add	a3,a6,a4
	sb	a2,0(a3)
	bne	a4,zero,.L58
.L50:
	ret
.L57:
	beq	a0,zero,.L50
	mv	a4,a6
	add	a2,a5,a0
.L60:
	lbu	a3,0(a5)
	addi	a5,a5,1
	addi	a4,a4,1
	sb	a3,-1(a4)
	bne	a2,a5,.L60
	ret
.L54:
	addiw	a3,a3,48
	addi	a5,a2,-1
	andi	a3,a3,0xff
	j	.L55
.L70:
	ret
.L61:
	mv	a2,a0
	j	.L52
.LFE43:
	.size	format_dec, .-format_dec
	.align	1
	.type	out_str, @function
out_str:
.LFB41:
	beq	a1,zero,.L85
	addi	sp,sp,-48
.LCFI7:
	sd	s3,8(sp)
.LCFI8:
	lla	s3,.LANCHOR1
	ld	a2,16(s3)
	sd	s0,32(sp)
	sd	s1,24(sp)
	sd	s2,16(sp)
	sd	s4,0(sp)
	sd	ra,40(sp)
.LCFI9:
	mv	s1,a1
	mv	s0,a0
	lla	s4,out_buf
	li	s2,8192
.L77:
	beq	a2,s2,.L73
	sub	a6,s2,a2
	bleu	a6,s1,.L75
	mv	a6,s1
.L75:
	mv	a0,s0
	add	a5,s4,a2
	add	a3,s0,a6
.L79:
	lbu	a4,0(a0)
	addi	a0,a0,1
	addi	a5,a5,1
	sb	a4,-1(a5)
	bne	a3,a0,.L79
	add	a2,a2,a6
	sd	a2,16(s3)
	sub	s1,s1,a6
	mv	s0,a3
	bne	s1,zero,.L77
	ld	ra,40(sp)
.LCFI10:
	ld	s0,32(sp)
.LCFI11:
	ld	s1,24(sp)
.LCFI12:
	ld	s2,16(sp)
.LCFI13:
	ld	s3,8(sp)
.LCFI14:
	ld	s4,0(sp)
.LCFI15:
	addi	sp,sp,48
.LCFI16:
	jr	ra
.L73:
.LCFI17:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	mv	a6,s1
	bgtu	s1,s2,.L88
	li	a2,0
	j	.L75
.L88:
	li	a6,8192
	li	a2,0
	j	.L75
.L85:
.LCFI18:
	ret
.LFE41:
	.size	out_str, .-out_str
	.align	1
	.type	out_char, @function
out_char:
.LFB40:
	addi	sp,sp,-32
.LCFI19:
	sd	s1,8(sp)
.LCFI20:
	lla	s1,.LANCHOR1
	ld	a4,16(s1)
	sd	s0,16(sp)
	sd	ra,24(sp)
.LCFI21:
	li	a5,8192
	mv	s0,a0
	beq	a4,a5,.L90
	addi	a3,a4,1
.L91:
	lla	a5,out_buf
	add	a5,a5,a4
	sb	s0,0(a5)
	ld	ra,24(sp)
.LCFI22:
	ld	s0,16(sp)
.LCFI23:
	sd	a3,16(s1)
	ld	s1,8(sp)
.LCFI24:
	addi	sp,sp,32
.LCFI25:
	jr	ra
.L90:
.LCFI26:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a3,1
	li	a4,0
	j	.L91
.LFE40:
	.size	out_char, .-out_char
	.align	1
	.globl	getint
	.type	getint, @function
getint:
.LFB50:
	addi	sp,sp,-32
.LCFI27:
	sd	s0,16(sp)
	sd	s1,8(sp)
	sd	ra,24(sp)
	sd	s2,0(sp)
.LCFI28:
	li	s0,32
	li	s1,4
.L110:
	call	in_get
	addiw	a5,a0,-9
	blt	a0,zero,.L104
	beq	a0,s0,.L110
	bleu	a5,s1,.L110
	li	a5,45
	bne	a0,a5,.L96
	call	in_get
	li	s2,1
	j	.L94
.L104:
	li	s2,0
.L94:
	addiw	a3,a0,-48
	li	a5,9
	mv	a4,a3
	bgtu	a3,a5,.L106
.L98:
	li	s0,0
	li	s1,9
.L101:
	slliw	a5,s0,2
	addw	a5,a5,s0
	slliw	a5,a5,1
	addw	s0,a4,a5
	call	in_get
	addiw	a5,a0,-48
	mv	a4,a5
	bleu	a5,s1,.L101
.L100:
	blt	a0,zero,.L102
.L99:
	sw	a0,.LANCHOR0,a5
.L102:
	beq	s2,zero,.L103
	negw	s0,s0
.L103:
	ld	ra,24(sp)
.LCFI29:
	mv	a0,s0
	ld	s0,16(sp)
.LCFI30:
	ld	s1,8(sp)
.LCFI31:
	ld	s2,0(sp)
.LCFI32:
	addi	sp,sp,32
.LCFI33:
	jr	ra
.L96:
.LCFI34:
	li	a5,43
	bne	a0,a5,.L113
	call	in_get
	li	s2,0
	j	.L94
.L113:
	addiw	a3,a0,-48
	li	a5,9
	mv	a4,a3
	li	s2,0
	li	s0,0
	bleu	a3,a5,.L98
	j	.L99
.L106:
	li	s0,0
	j	.L100
.LFE50:
	.size	getint, .-getint
	.align	1
	.globl	getch
	.type	getch, @function
getch:
.LFB51:
	lla	a5,.LANCHOR0
	lw	a0,0(a5)
	blt	a0,zero,.L115
	li	a4,-1
	sw	a4,0(a5)
	ret
.L115:
	addi	sp,sp,-16
.LCFI35:
	sd	s0,0(sp)
.LCFI36:
	lla	s0,.LANCHOR1
	ld	a5,0(s0)
	ld	a0,8(s0)
	sd	ra,8(sp)
.LCFI37:
	beq	a5,a0,.L122
.L117:
	bleu	a0,a5,.L118
	lla	a4,in_buf
	addi	a3,a5,1
	add	a5,a4,a5
	lbu	a0,0(a5)
	sd	a3,0(s0)
.L116:
	ld	ra,8(sp)
.LCFI38:
	ld	s0,0(sp)
.LCFI39:
	addi	sp,sp,16
.LCFI40:
	jr	ra
.L122:
.LCFI41:
	la	a5,stdin
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,in_buf
	call	fread@plt
	sd	zero,.LANCHOR1,a5
	sd	a0,8(s0)
	li	a5,0
	j	.L117
.L118:
	li	a0,0
	j	.L116
.LFE51:
	.size	getch, .-getch
	.globl	__clzdi2
	.align	1
	.globl	getfloat
	.type	getfloat, @function
getfloat:
.LFB54:
	addi	sp,sp,-160
.LCFI42:
	sd	s2,128(sp)
.LCFI43:
	la	s2,__stack_chk_guard
	sd	s0,144(sp)
	sd	s1,136(sp)
	sd	ra,152(sp)
	sd	s3,120(sp)
	sd	s4,112(sp)
	ld	a5, 0(s2)
	sd	a5, 72(sp)
	li	a5, 0
.LCFI44:
	li	s0,32
	li	s1,4
.L281:
	call	in_get
	blt	a0,zero,.L124
	beq	a0,s0,.L281
	addiw	a5,a0,-9
	bleu	a5,s1,.L281
	li	s0,0
	li	s1,63
	li	s3,32
	li	s4,4
.L126:
	addi	a5,s0,1
	bgtu	a5,s1,.L132
	addi	a4,s0,80
	add	s0,a4,sp
	sb	a0,-72(s0)
	mv	s0,a5
.L132:
	call	in_get
	blt	a0,zero,.L128
	beq	a0,s3,.L130
	addiw	a5,a0,-9
	bgtu	a5,s4,.L126
.L130:
	sw	a0,.LANCHOR0,a5
.L128:
	lbu	t6,8(sp)
	addi	a5,s0,80
	add	s0,a5,sp
	addiw	a5,t6,-43
	sb	zero,-72(s0)
	andi	a5,a5,253
	beq	a5,zero,.L220
	ori	a5,t6,32
	li	a4,105
	beq	a5,a4,.L221
	li	a4,110
	bne	a5,a4,.L136
	lbu	a4,9(sp)
	li	a3,97
	ori	a4,a4,32
	bne	a4,a3,.L136
	lbu	a4,10(sp)
	ori	a4,a4,32
	beq	a4,a5,.L139
.L136:
	li	a5,48
	bne	t6,a5,.L229
	lbu	a5,9(sp)
	li	a4,120
	ori	a5,a5,32
	beq	a5,a4,.L230
	li	t1,0
	addi	a5,sp,8
.L144:
	li	a4,0
	j	.L223
.L220:
	lbu	a4,9(sp)
	li	a3,105
	ori	a5,a4,32
	beq	a5,a3,.L306
	li	a3,110
	bne	a5,a3,.L137
	lbu	a3,10(sp)
	li	a2,97
	ori	a3,a3,32
	beq	a3,a2,.L307
.L137:
	li	a5,48
	bne	a4,a5,.L308
	lbu	a4,10(sp)
	addi	t1,t6,-45
	li	a3,120
	ori	a4,a4,32
	seqz	t1,t1
	addi	a5,sp,9
	bne	a4,a3,.L144
	addi	a5,t6,-45
	seqz	s0,a5
	slli	s0,s0,32
	sd	s5,104(sp)
	sd	s6,96(sp)
	addi	a5,sp,9
	srli	s0,s0,32
.LCFI45:
.L143:
	addi	a5,a5,2
	li	s5,0
	li	s6,-1
	li	s3,0
	li	s4,0
	li	s1,0
	li	a1,9
	li	a0,5
.L158:
	lbu	a3,0(a5)
	addiw	a2,a3,-48
	sext.w	a4,a3
	bleu	a2,a1,.L309
	addiw	a2,a4,-97
	bleu	a2,a0,.L310
	addiw	a2,a4,-65
	bgtu	a2,a0,.L152
	addiw	a4,a4,-55
	blt	s6,zero,.L150
.L296:
	bgt	s4,a1,.L300
.L153:
	slli	s1,s1,4
	add	s1,a4,s1
	addiw	s4,s4,1
.L154:
	addiw	s3,s3,1
	addi	a5,a5,1
	j	.L158
.L183:
.LCFI46:
	li	a5,-40
	bge	a4,a5,.L185
	bne	s0,zero,.L237
	ld	s5,104(sp)
.LCFI47:
	ld	s6,96(sp)
.LCFI48:
	ld	s7,88(sp)
.LCFI49:
.L124:
	mv	a5,zero
.L123:
	ld	a3, 72(sp)
	ld	a4, 0(s2)
	xor	a4, a3, a4
	li	a3, 0
	bne	a4,zero,.L311
	ld	ra,152(sp)
.LCFI50:
	ld	s0,144(sp)
.LCFI51:
	ld	s1,136(sp)
.LCFI52:
	ld	s2,128(sp)
.LCFI53:
	ld	s3,120(sp)
.LCFI54:
	ld	s4,112(sp)
.LCFI55:
	fmv.s.x	fa0,a5
	addi	sp,sp,160
.LCFI56:
	jr	ra
.L308:
.LCFI57:
	addi	a5,t6,-45
	seqz	t1,a5
	mv	t6,a4
	addi	a5,sp,9
.L142:
	addiw	a4,t6,-48
	andi	a2,a4,0xff
	li	a3,9
	bgtu	a2,a3,.L239
.L223:
	li	t3,0
	li	a7,0
	li	a0,0
	li	a1,0
	li	a6,0
	li	t5,0
	li	t4,18
	li	a2,9
	j	.L195
.L312:
	ble	a1,t4,.L193
	addiw	a7,a7,1
.L194:
	lbu	t6,1(a5)
	addi	a5,a5,1
	addiw	a4,t6,-48
	andi	a3,a4,0xff
	bgtu	a3,a2,.L191
.L195:
	sext.w	a4,a4
	or	t5,t5,a4
	beq	t5,zero,.L312
	addiw	a6,a6,1
	ble	a1,t4,.L240
	addiw	a7,a7,1
	li	t5,1
	beq	a4,zero,.L194
	lbu	t6,1(a5)
	li	t3,1
	addi	a5,a5,1
	addiw	a4,t6,-48
	andi	a3,a4,0xff
	bleu	a3,a2,.L195
.L191:
	li	a4,46
	beq	t6,a4,.L313
	lbu	a3,0(a5)
	li	t4,0
	li	t6,0
.L197:
	andi	a4,a3,223
	li	a2,69
	li	a3,0
	beq	a4,a2,.L314
.L202:
	beq	a1,zero,.L124
	li	a5,9
	bgt	a6,a5,.L209
	bne	t3,zero,.L209
	addw	a4,a7,a3
	fcvt.d.lu	fa5,a0
	mv	a7,a4
	beq	t4,zero,.L211
	ble	t4,zero,.L249
	li	a5,0
	fld	fa4,.LC0,a3
	fld	fa3,.LC2,a3
.L213:
	addiw	a5,a5,1
	fmul.d	fa4,fa4,fa3
	bne	t4,a5,.L213
.L212:
	fcvt.d.lu	fa3,t6
	fdiv.d	fa4,fa3,fa4
	fadd.d	fa5,fa5,fa4
.L211:
	li	a5,308
	bgt	a4,a5,.L315
	li	a5,-324
	blt	a4,a5,.L316
	ble	a4,zero,.L216
	fld	fa3,.LC2,a5
.L217:
	addiw	t3,t3,1
	fmul.d	fa5,fa5,fa3
	bne	a4,t3,.L217
.L218:
	fcvt.s.d	fa5,fa5
	fmv.x.s	a5,fa5
	beq	t1,zero,.L123
	fneg.s	fa5,fa5
	fmv.x.s	a5,fa5
	j	.L123
.L240:
	li	t5,1
.L193:
	slli	a3,a0,2
	add	a3,a3,a0
	slli	a3,a3,1
	add	a0,a4,a3
	addiw	a1,a1,1
	j	.L194
.L314:
	lbu	a4,1(a5)
	li	a2,45
	beq	a4,a2,.L317
	li	a2,43
	beq	a4,a2,.L318
	addiw	a4,a4,-48
	andi	t5,a4,0xff
	li	a2,9
	bgtu	t5,a2,.L202
	addi	a5,a5,1
	li	t2,0
.L204:
	li	t5,98304
	li	a3,0
	addi	t5,t5,1695
	li	t0,9
.L208:
	bgt	a3,t5,.L207
	slliw	a2,a3,2
	addw	a3,a2,a3
	slliw	a3,a3,1
	addw	a3,a4,a3
.L207:
	lbu	a4,1(a5)
	addi	a5,a5,1
	addiw	a4,a4,-48
	andi	a2,a4,0xff
	bleu	a2,t0,.L208
	beq	t2,zero,.L202
	negw	a3,a3
	j	.L202
.L221:
	lbu	a5,9(sp)
	li	a4,110
	ori	a5,a5,32
	bne	a5,a4,.L136
	lbu	a5,10(sp)
	li	a4,102
	ori	a5,a5,32
	bne	a5,a4,.L136
.L135:
	addi	a5,t6,-45
	seqz	a5,a5
	li	a4,2139095040
.L140:
	slliw	a5,a5,31
	or	a5,a5,a4
	j	.L123
.L313:
	lbu	a3,1(a5)
	li	a2,9
	addi	a5,a5,1
	addiw	a4,a3,-48
	andi	t4,a4,0xff
	bgtu	t4,a2,.L242
	li	t6,0
	li	t4,0
	li	t2,18
	li	t0,9
	j	.L201
.L320:
	bgt	t4,t2,.L200
.L199:
	slli	a3,t6,2
	add	a3,a3,t6
	slli	a3,a3,1
	add	t6,a4,a3
	addiw	t4,t4,1
.L200:
	lbu	a3,1(a5)
	addi	a5,a5,1
	addiw	a4,a3,-48
	andi	a2,a4,0xff
	bgtu	a2,t0,.L319
.L201:
	sext.w	a4,a4
	or	t5,a4,t5
	beq	t5,zero,.L320
	addiw	a6,a6,1
	li	t5,1
	ble	t4,t2,.L199
	beq	a4,zero,.L200
	lbu	a3,1(a5)
	li	t3,1
	addi	a5,a5,1
	addiw	a4,a3,-48
	andi	a2,a4,0xff
	bleu	a2,t0,.L201
.L319:
	or	a1,t4,a1
	j	.L197
.L209:
	li	a1,0
	addi	a0,sp,8
	call	strtof
	fmv.x.s	a5,fa0
	j	.L123
.L229:
	li	t1,0
	addi	a5,sp,8
	j	.L142
.L317:
	lbu	a4,2(a5)
	li	a2,9
	addi	a5,a5,2
	addiw	a4,a4,-48
	andi	t5,a4,0xff
	li	t2,1
	bleu	t5,a2,.L204
	j	.L202
.L306:
	lbu	a5,10(sp)
	li	a3,110
	ori	a5,a5,32
	bne	a5,a3,.L137
	lbu	a5,11(sp)
	li	a3,102
	ori	a5,a5,32
	beq	a5,a3,.L135
	j	.L137
.L318:
	lbu	a4,2(a5)
	li	a2,9
	addi	a5,a5,2
	addiw	a4,a4,-48
	andi	t5,a4,0xff
	li	t2,0
	bleu	t5,a2,.L204
	j	.L202
.L216:
	beq	a4,zero,.L218
	negw	a7,a7
	fld	fa3,.LC2,a5
.L219:
	addiw	t3,t3,1
	fdiv.d	fa5,fa5,fa3
	bne	t3,a7,.L219
	j	.L218
.L310:
.LCFI58:
	addiw	a4,a4,-87
	bge	s6,zero,.L296
.L150:
	mv	s1,a4
	mv	s6,s3
	li	s4,1
	j	.L154
.L315:
.LCFI59:
	slliw	a5,t1,31
	li	a4,2139095040
	or	a5,a5,a4
	j	.L123
.L239:
	li	t3,0
	li	a7,0
	li	a0,0
	li	a1,0
	li	a6,0
	li	t5,0
	j	.L191
.L316:
	slliw	a5,t1,31
	j	.L123
.L309:
.LCFI60:
	mv	a4,a2
	bge	s6,zero,.L321
	beq	a2,zero,.L154
	j	.L150
.L242:
.LCFI61:
	li	t4,0
	li	t6,0
	j	.L197
.L230:
	sd	s5,104(sp)
	sd	s6,96(sp)
	li	s0,0
	addi	a5,sp,8
.LCFI62:
	j	.L143
.L307:
.LCFI63:
	lbu	a3,11(sp)
	ori	a3,a3,32
	bne	a3,a5,.L137
.L139:
	addi	a5,t6,-45
	seqz	a5,a5
	li	a4,2143289344
	j	.L140
.L321:
.LCFI64:
	ble	s4,a1,.L153
	beq	a2,zero,.L154
.L300:
	li	s5,1
	j	.L154
.L249:
.LCFI65:
	fld	fa4,.LC0,a5
	j	.L212
.L152:
.LCFI66:
	li	a4,46
	beq	a3,a4,.L322
.L156:
	blt	s6,zero,.L323
	sd	s7,88(sp)
	lbu	a4,0(a5)
	li	a3,80
.LCFI67:
	li	s7,0
	andi	a4,a4,223
	beq	a4,a3,.L324
.L170:
	mv	a0,s1
	call	__clzdi2@plt
	subw	a5,s3,s6
	subw	a5,a5,s4
	slliw	a5,a5,2
	addw	a5,a5,s7
	subw	a5,a5,a0
	addiw	a6,a5,190
	li	a4,254
	bgt	a6,a4,.L302
	addiw	a4,a0,-40
	mv	a1,a4
	ble	a6,zero,.L178
	blt	a4,zero,.L179
	sll	a5,s1,a4
	li	a4,16777216
	beq	a5,a4,.L180
.L181:
	slli	a4,a5,41
	srli	a4,a4,41
	slliw	a5,a6,23
	or	a4,a4,a5
	slliw	a5,s0,31
	ld	s5,104(sp)
.LCFI68:
	ld	s6,96(sp)
.LCFI69:
	ld	s7,88(sp)
.LCFI70:
	or	a5,a5,a4
	j	.L123
.L322:
.LCFI71:
	addi	a5,a5,1
	li	a3,9
	subw	a0,s3,a5
	li	a1,5
.L168:
	lbu	a4,0(a5)
	addiw	a2,a4,-48
	bleu	a2,a3,.L325
	addiw	a2,a4,-97
	bleu	a2,a1,.L326
	addiw	a2,a4,-65
	bgtu	a2,a1,.L156
	addiw	a4,a4,-55
	blt	s6,zero,.L163
.L298:
	bgt	s4,a3,.L301
.L165:
	slli	s1,s1,4
	add	s1,a4,s1
	addiw	s4,s4,1
.L166:
	addi	a5,a5,1
	j	.L168
.L326:
	addiw	a4,a4,-87
	bge	s6,zero,.L298
.L163:
	addw	s6,a0,a5
	mv	s1,a4
	li	s4,1
	addi	a5,a5,1
	j	.L168
.L325:
	mv	a4,a2
	bge	s6,zero,.L327
	bne	a2,zero,.L163
	addi	a5,a5,1
	j	.L168
.L327:
	ble	s4,a3,.L165
	beq	a2,zero,.L166
.L301:
	li	s5,1
	addi	a5,a5,1
	j	.L168
.L324:
.LCFI72:
	lbu	a4,1(a5)
	li	a3,45
	beq	a4,a3,.L328
	li	a3,43
	beq	a4,a3,.L329
	addiw	a4,a4,-48
	andi	a2,a4,0xff
	li	a3,9
	bgtu	a2,a3,.L170
	addi	a5,a5,1
	li	a1,0
.L172:
	li	a2,98304
	li	s7,0
	addi	a2,a2,1695
	li	a0,9
.L176:
	bgt	s7,a2,.L175
	slliw	a3,s7,2
	addw	a3,a3,s7
	slliw	a3,a3,1
	addw	s7,a4,a3
.L175:
	lbu	a4,1(a5)
	addi	a5,a5,1
	addiw	a4,a4,-48
	andi	a3,a4,0xff
	bleu	a3,a0,.L176
	beq	a1,zero,.L170
	negw	s7,s7
	j	.L170
.L323:
.LCFI73:
	ld	s5,104(sp)
.LCFI74:
	ld	s6,96(sp)
.LCFI75:
	slliw	a5,s0,31
	j	.L123
.L180:
.LCFI76:
	addiw	a6,a6,1
	li	a4,255
	li	a5,8388608
	bne	a6,a4,.L181
.L302:
	slliw	a5,s0,31
	li	a4,2139095040
	ld	s5,104(sp)
.LCFI77:
	ld	s6,96(sp)
.LCFI78:
	ld	s7,88(sp)
.LCFI79:
	or	a5,a5,a4
	j	.L123
.L178:
.LCFI80:
	li	a3,1
	subw	a3,a3,a6
	subw	a4,a4,a3
	blt	a4,zero,.L183
	sll	a5,s1,a4
	li	a4,16777216
	beq	a5,a4,.L330
.L184:
	li	a4,8388608
	sltu	a4,a5,a4
	xori	a6,a4,1
	j	.L181
.L179:
	li	a3,40
	subw	a3,a3,a0
.L182:
	li	a2,-1
	sll	a1,a2,a3
	not	a1,a1
	and	a1,a1,s1
	not	a4,a4
	srl	a0,a1,a4
	srl	a5,s1,a3
	andi	a3,a0,1
	bne	s5,zero,.L186
	sll	a4,a2,a4
	not	a4,a4
	and	a4,a4,a1
	bne	a4,zero,.L186
	beq	a3,zero,.L189
	andi	a4,a5,1
	bne	a4,zero,.L188
.L189:
	li	a4,16777216
	beq	a5,a4,.L180
	bne	a6,zero,.L181
	j	.L184
.L186:
	beq	a3,zero,.L189
.L188:
	addi	a5,a5,1
	j	.L189
.L329:
	lbu	a4,2(a5)
	li	a3,9
	addi	a5,a5,2
	addiw	a4,a4,-48
	andi	a2,a4,0xff
	li	a1,0
	bleu	a2,a3,.L172
	j	.L170
.L328:
	lbu	a4,2(a5)
	li	a3,9
	addi	a5,a5,2
	addiw	a4,a4,-48
	andi	a2,a4,0xff
	li	a1,1
	bleu	a2,a3,.L172
	negw	s7,s7
	j	.L170
.L330:
	li	a5,8388608
	li	a6,1
	j	.L181
.L311:
.LCFI81:
	sd	s5,104(sp)
	sd	s6,96(sp)
	sd	s7,88(sp)
.LCFI82:
	call	__stack_chk_fail@plt
.L237:
	ld	s5,104(sp)
.LCFI83:
	ld	s6,96(sp)
.LCFI84:
	ld	s7,88(sp)
.LCFI85:
	lw	a5,.LC1
	j	.L123
.L185:
.LCFI86:
	subw	a3,a3,a1
	li	a6,0
	j	.L182
.LFE54:
	.size	getfloat, .-getfloat
	.align	1
	.globl	getarray
	.type	getarray, @function
getarray:
.LFB55:
	addi	sp,sp,-32
.LCFI87:
	sd	s0,16(sp)
	sd	s2,0(sp)
	sd	ra,24(sp)
.LCFI88:
	mv	s0,a0
	call	getint
	mv	s2,a0
	ble	a0,zero,.L332
	sd	s1,8(sp)
.LCFI89:
	slli	s1,a0,2
	add	s1,s0,s1
.L333:
	call	getint
	sw	a0,0(s0)
	addi	s0,s0,4
	bne	s0,s1,.L333
	ld	s1,8(sp)
.LCFI90:
.L332:
	ld	ra,24(sp)
.LCFI91:
	ld	s0,16(sp)
.LCFI92:
	mv	a0,s2
	ld	s2,0(sp)
.LCFI93:
	addi	sp,sp,32
.LCFI94:
	jr	ra
.LFE55:
	.size	getarray, .-getarray
	.align	1
	.globl	getfarray
	.type	getfarray, @function
getfarray:
.LFB56:
	addi	sp,sp,-32
.LCFI95:
	sd	s0,16(sp)
	sd	s2,0(sp)
	sd	ra,24(sp)
.LCFI96:
	mv	s0,a0
	call	getint
	mv	s2,a0
	ble	a0,zero,.L337
	sd	s1,8(sp)
.LCFI97:
	slli	s1,a0,2
	add	s1,s0,s1
.L338:
	call	getfloat
	addi	s0,s0,4
	fsw	fa0,-4(s0)
	bne	s0,s1,.L338
	ld	s1,8(sp)
.LCFI98:
.L337:
	ld	ra,24(sp)
.LCFI99:
	ld	s0,16(sp)
.LCFI100:
	mv	a0,s2
	ld	s2,0(sp)
.LCFI101:
	addi	sp,sp,32
.LCFI102:
	jr	ra
.LFE56:
	.size	getfarray, .-getfarray
	.align	1
	.globl	putint
	.type	putint, @function
putint:
.LFB57:
	addi	sp,sp,-64
.LCFI103:
	sd	s0,48(sp)
	sd	s1,40(sp)
.LCFI104:
	addi	s0,sp,8
	la	s1,__stack_chk_guard
	mv	a1,a0
	ld	a5, 0(s1)
	sd	a5, 24(sp)
	li	a5, 0
	mv	a0,s0
	sd	ra,56(sp)
.LCFI105:
	call	format_dec
	mv	a1,a0
	mv	a0,s0
	call	out_str
	ld	a4, 24(sp)
	ld	a5, 0(s1)
	xor	a5, a4, a5
	li	a4, 0
	bne	a5,zero,.L344
	ld	ra,56(sp)
.LCFI106:
	ld	s0,48(sp)
.LCFI107:
	ld	s1,40(sp)
.LCFI108:
	addi	sp,sp,64
.LCFI109:
	jr	ra
.L344:
.LCFI110:
	call	__stack_chk_fail@plt
.LFE57:
	.size	putint, .-putint
	.align	1
	.globl	putch
	.type	putch, @function
putch:
.LFB58:
	addi	sp,sp,-32
.LCFI111:
	sd	s1,8(sp)
.LCFI112:
	lla	s1,.LANCHOR1
	ld	a4,16(s1)
	sd	s0,16(sp)
	sd	ra,24(sp)
.LCFI113:
	li	a5,8192
	andi	s0,a0,0xff
	beq	a4,a5,.L346
	addi	a3,a4,1
.L347:
	lla	a5,out_buf
	add	a5,a5,a4
	sb	s0,0(a5)
	ld	ra,24(sp)
.LCFI114:
	ld	s0,16(sp)
.LCFI115:
	sd	a3,16(s1)
	ld	s1,8(sp)
.LCFI116:
	addi	sp,sp,32
.LCFI117:
	jr	ra
.L346:
.LCFI118:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a3,1
	li	a4,0
	j	.L347
.LFE58:
	.size	putch, .-putch
	.align	1
	.globl	putarray
	.type	putarray, @function
putarray:
.LFB59:
	addi	sp,sp,-112
.LCFI119:
	sd	s1,88(sp)
	sd	s4,64(sp)
.LCFI120:
	addi	s1,sp,8
	la	s4,__stack_chk_guard
	sd	s0,96(sp)
	sd	s3,72(sp)
	ld	a5, 0(s4)
	sd	a5, 24(sp)
	li	a5, 0
.LCFI121:
	mv	s3,a0
	mv	s0,a1
	mv	a1,a0
	mv	a0,s1
	sd	ra,104(sp)
	sd	s2,80(sp)
	sd	s7,40(sp)
.LCFI122:
	call	format_dec
	mv	a1,a0
	mv	a0,s1
	call	out_str
	li	a0,58
	call	out_char
	ble	s3,zero,.L362
	slli	s3,s3,2
	sd	s5,56(sp)
	sd	s6,48(sp)
	add	s3,s0,s3
	lla	s2,.LANCHOR1
	lla	s7,out_buf
.LCFI123:
	li	s6,8192
	li	s5,32
	j	.L355
.L364:
	addi	a4,a5,1
.L354:
	add	a5,s7,a5
	sb	s5,0(a5)
	lw	a1,0(s0)
	mv	a0,s1
	sd	a4,16(s2)
	call	format_dec
	mv	a1,a0
	addi	s0,s0,4
	mv	a0,s1
	call	out_str
	beq	s0,s3,.L363
.L355:
	ld	a5,16(s2)
	bne	a5,s6,.L364
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a4,1
	li	a5,0
	j	.L354
.L363:
	ld	a5,16(s2)
	li	a4,8192
	ld	s5,56(sp)
.LCFI124:
	ld	s6,48(sp)
.LCFI125:
	beq	a5,a4,.L365
.L360:
	addi	a4,a5,1
.L357:
	add	a5,s7,a5
	sd	a4,16(s2)
	li	a4,10
	sb	a4,0(a5)
	ld	a4, 24(sp)
	ld	a5, 0(s4)
	xor	a5, a4, a5
	li	a4, 0
	bne	a5,zero,.L366
	ld	ra,104(sp)
.LCFI126:
	ld	s0,96(sp)
.LCFI127:
	ld	s1,88(sp)
.LCFI128:
	ld	s2,80(sp)
.LCFI129:
	ld	s3,72(sp)
.LCFI130:
	ld	s4,64(sp)
.LCFI131:
	ld	s7,40(sp)
.LCFI132:
	addi	sp,sp,112
.LCFI133:
	jr	ra
.L362:
.LCFI134:
	lla	s2,.LANCHOR1
	ld	a5,16(s2)
	li	a4,8192
	lla	s7,out_buf
	bne	a5,a4,.L360
.L365:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a4,1
	li	a5,0
	j	.L357
.L366:
	sd	s5,56(sp)
	sd	s6,48(sp)
.LCFI135:
	call	__stack_chk_fail@plt
.LFE59:
	.size	putarray, .-putarray
	.align	1
	.globl	putfloat
	.type	putfloat, @function
putfloat:
.LFB60:
	addi	sp,sp,-80
.LCFI136:
	sd	s0,64(sp)
	sd	s1,56(sp)
.LCFI137:
	addi	s0,sp,8
	la	s1,__stack_chk_guard
	ld	a5, 0(s1)
	sd	a5, 40(sp)
	li	a5, 0
	mv	a0,s0
	sd	ra,72(sp)
.LCFI138:
	call	format_hex_float
	mv	a1,a0
	mv	a0,s0
	call	out_str
	ld	a4, 40(sp)
	ld	a5, 0(s1)
	xor	a5, a4, a5
	li	a4, 0
	bne	a5,zero,.L370
	ld	ra,72(sp)
.LCFI139:
	ld	s0,64(sp)
.LCFI140:
	ld	s1,56(sp)
.LCFI141:
	addi	sp,sp,80
.LCFI142:
	jr	ra
.L370:
.LCFI143:
	call	__stack_chk_fail@plt
.LFE60:
	.size	putfloat, .-putfloat
	.align	1
	.globl	putfarray
	.type	putfarray, @function
putfarray:
.LFB61:
	addi	sp,sp,-144
.LCFI144:
	sd	s1,120(sp)
	sd	s4,96(sp)
.LCFI145:
	addi	s1,sp,8
	la	s4,__stack_chk_guard
	ld	a5, 0(s4)
	sd	a5, 56(sp)
	li	a5, 0
	sd	s0,128(sp)
	sd	s2,112(sp)
.LCFI146:
	mv	s0,a1
	mv	s2,a0
	mv	a1,a0
	mv	a0,s1
	sd	ra,136(sp)
	sd	s3,104(sp)
.LCFI147:
	call	format_dec
	mv	a1,a0
	mv	a0,s1
	call	out_str
	lla	s1,.LANCHOR1
	ld	a4,16(s1)
	li	a5,8192
	beq	a4,a5,.L372
	addi	a5,a4,1
.L373:
	lla	s3,out_buf
	add	a4,s3,a4
	li	a3,58
	sd	a5,16(s1)
	sb	a3,0(a4)
	ble	s2,zero,.L374
	sd	s5,88(sp)
	sd	s6,80(sp)
	sd	s7,72(sp)
	slli	s2,s2,2
.LCFI148:
	li	s7,8192
	add	s2,s0,s2
	addi	s5,sp,24
	li	s6,32
	beq	a5,s7,.L375
.L382:
	addi	a4,a5,1
.L376:
	add	a5,s3,a5
	sb	s6,0(a5)
	flw	fa0,0(s0)
	mv	a0,s5
	sd	a4,16(s1)
	call	format_hex_float
	mv	a1,a0
	addi	s0,s0,4
	mv	a0,s5
	call	out_str
	beq	s0,s2,.L381
	ld	a5,16(s1)
	bne	a5,s7,.L382
.L375:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a4,1
	li	a5,0
	j	.L376
.L381:
	ld	s5,88(sp)
.LCFI149:
	ld	s6,80(sp)
.LCFI150:
	ld	s7,72(sp)
.LCFI151:
.L374:
	ld	a4, 56(sp)
	ld	a5, 0(s4)
	xor	a5, a4, a5
	li	a4, 0
	bne	a5,zero,.L383
	ld	s0,128(sp)
.LCFI152:
	ld	ra,136(sp)
.LCFI153:
	ld	s1,120(sp)
.LCFI154:
	ld	s2,112(sp)
.LCFI155:
	ld	s3,104(sp)
.LCFI156:
	ld	s4,96(sp)
.LCFI157:
	li	a0,10
	addi	sp,sp,144
.LCFI158:
	tail	out_char
.L372:
.LCFI159:
	la	a5,stdout
	ld	a3,0(a5)
	li	a2,8192
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	li	a5,1
	li	a4,0
	j	.L373
.L383:
	sd	s5,88(sp)
	sd	s6,80(sp)
	sd	s7,72(sp)
.LCFI160:
	call	__stack_chk_fail@plt
.LFE61:
	.size	putfarray, .-putfarray
	.align	1
	.globl	putf
	.type	putf, @function
putf:
.LFB62:
	addi	sp,sp,-112
.LCFI161:
	sd	a2,64(sp)
	sd	s1,24(sp)
	sd	a1,56(sp)
.LCFI162:
	la	s1,__stack_chk_guard
	sd	a3,72(sp)
	sd	a4,80(sp)
	sd	a5,88(sp)
	sd	a6,96(sp)
	sd	a7,104(sp)
	ld	a5, 0(s1)
	sd	a5, 8(sp)
	li	a5, 0
	addi	a5,sp,56
	sd	s0,32(sp)
	sd	s2,16(sp)
	sd	ra,40(sp)
.LCFI163:
	sd	a5,0(sp)
	ld	a2,.LANCHOR1+16
	mv	s0,a0
	la	s2,stdout
	bne	a2,zero,.L391
.L385:
	ld	a3,0(sp)
	ld	a0,0(s2)
	mv	a2,s0
	li	a1,2
	call	__vfprintf_chk@plt
	ld	a4, 8(sp)
	ld	a5, 0(s1)
	xor	a5, a4, a5
	li	a4, 0
	bne	a5,zero,.L392
	ld	ra,40(sp)
.LCFI164:
	ld	s0,32(sp)
.LCFI165:
	ld	s1,24(sp)
.LCFI166:
	ld	s2,16(sp)
.LCFI167:
	addi	sp,sp,112
.LCFI168:
	jr	ra
.L391:
.LCFI169:
	ld	a3,0(s2)
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	sd	zero,.LANCHOR1+16,a5
	j	.L385
.L392:
	call	__stack_chk_fail@plt
.LFE62:
	.size	putf, .-putf
	.section	.text.startup,"ax",@progbits
	.align	1
	.globl	before_main
	.type	before_main, @function
before_main:
.LFB63:
	lla	a5,_sysy_us
	lla	a2,_sysy_s
	lla	a3,_sysy_m
	lla	a4,_sysy_h
	lla	a1,_sysy_us+4096
.L394:
	sw	zero,0(a5)
	sw	zero,0(a2)
	sw	zero,0(a3)
	sw	zero,0(a4)
	addi	a5,a5,4
	addi	a2,a2,4
	addi	a3,a3,4
	addi	a4,a4,4
	bne	a5,a1,.L394
	li	a5,1
	sw	a5,.LANCHOR1+24,a4
	ret
.LFE63:
	.size	before_main, .-before_main
	.section	.init_array,"aw"
	.align	3
	.dword	before_main
	.section	.rodata.str1.8,"aMS",@progbits,1
	.align	3
.LC3:
	.string	"Timer@%04d-%04d: %dH-%dM-%dS-%dus\n"
	.align	3
.LC4:
	.string	"TOTAL: %dH-%dM-%dS-%dus\n"
	.section	.text.exit,"ax",@progbits
	.align	1
	.globl	after_main
	.type	after_main, @function
after_main:
.LFB64:
	lla	a5,.LANCHOR1
	ld	a2,16(a5)
	addi	sp,sp,-144
.LCFI170:
	sd	ra,136(sp)
.LCFI171:
	bne	a2,zero,.L406
.L397:
	lla	a5,.LANCHOR1
	lw	a4,24(a5)
	li	a5,1
	ble	a4,a5,.L398
	li	a5,999424
	la	a4,stderr
	addiw	a5,a5,576
	sd	s0,128(sp)
	sd	s1,120(sp)
	sd	s2,112(sp)
	sd	s3,104(sp)
	sd	s4,96(sp)
	sd	s5,88(sp)
	sd	s6,80(sp)
	sd	s7,72(sp)
	sd	s8,64(sp)
	sd	s9,56(sp)
	sd	s10,48(sp)
	sd	s11,40(sp)
.LCFI172:
	lla	s10,_sysy_l1+4
	lla	s9,_sysy_l2+4
	lla	s3,_sysy_h+4
	lla	s2,_sysy_m+4
	lla	s1,_sysy_s+4
	lla	s0,_sysy_us+4
	li	s4,1
	sd	a4,16(sp)
	lla	s5,_sysy_s
	lla	s6,_sysy_us
	lla	s7,_sysy_m
	lla	s8,_sysy_h
	sw	a5,28(sp)
	li	s11,60
.L399:
	ld	a1,16(sp)
	lw	a2,0(s0)
	lw	a6,0(s2)
	lw	a5,0(s3)
	lw	a4,0(s9)
	lw	a3,0(s10)
	lw	a7,0(s1)
	ld	a0,0(a1)
	sd	a2,0(sp)
	li	a1,2
	lla	a2,.LC3
	call	__fprintf_chk@plt
	lw	a4,0(s2)
	lw	a3,0(s7)
	lw	a5,0(s1)
	lw	a6,0(s0)
	lw	a1,0(s5)
	lw	a2,0(s6)
	addw	a4,a4,a3
	lw	a3,28(sp)
	addw	a5,a5,a1
	addw	a6,a6,a2
	remw	a6,a6,a3
	lw	a1,0(s8)
	lw	a3,0(s3)
	lla	a2,.LANCHOR1
	lw	a2,24(a2)
	addw	a3,a3,a1
	addiw	s4,s4,1
	sw	a3,0(s8)
	addi	s10,s10,4
	addi	s9,s9,4
	addi	s3,s3,4
	addi	s2,s2,4
	addi	s1,s1,4
	addi	s0,s0,4
	remw	a5,a5,s11
	sw	a6,0(s6)
	remw	a4,a4,s11
	sw	a5,0(s5)
	sw	a4,0(s7)
	bgt	a2,s4,.L399
	ld	s0,128(sp)
.LCFI173:
	ld	s1,120(sp)
.LCFI174:
	ld	s2,112(sp)
.LCFI175:
	ld	s3,104(sp)
.LCFI176:
	ld	s4,96(sp)
.LCFI177:
	ld	s5,88(sp)
.LCFI178:
	ld	s6,80(sp)
.LCFI179:
	ld	s7,72(sp)
.LCFI180:
	ld	s8,64(sp)
.LCFI181:
	ld	s9,56(sp)
.LCFI182:
	ld	s10,48(sp)
.LCFI183:
	ld	s11,40(sp)
.LCFI184:
.L400:
	ld	a2,16(sp)
	ld	ra,136(sp)
.LCFI185:
	li	a1,2
	ld	a0,0(a2)
	lla	a2,.LC4
	addi	sp,sp,144
.LCFI186:
	tail	__fprintf_chk@plt
.L406:
.LCFI187:
	la	a5,stdout
	ld	a3,0(a5)
	li	a1,1
	lla	a0,out_buf
	call	fwrite@plt
	sd	zero,.LANCHOR1+16,a5
	j	.L397
.L398:
	la	a2,stderr
	lw	a3,_sysy_h
	lw	a4,_sysy_m
	lw	a5,_sysy_s
	lw	a6,_sysy_us
	sd	a2,16(sp)
	j	.L400
.LFE64:
	.size	after_main, .-after_main
	.section	.fini_array,"aw"
	.align	3
	.dword	after_main
	.text
	.align	1
	.globl	_sysy_starttime
	.type	_sysy_starttime, @function
_sysy_starttime:
.LFB65:
	lw	a4,.LANCHOR1+24
	slli	a4,a4,2
	lla	a5,_sysy_l1
	mv	a3,a0
	add	a5,a5,a4
	li	a1,0
	lla	a0,.LANCHOR1+32
	sw	a3,0(a5)
	tail	gettimeofday@plt
.LFE65:
	.size	_sysy_starttime, .-_sysy_starttime
	.align	1
	.globl	_sysy_stoptime
	.type	_sysy_stoptime, @function
_sysy_stoptime:
.LFB66:
	addi	sp,sp,-32
.LCFI188:
	sd	s1,8(sp)
	li	a1,0
.LCFI189:
	mv	s1,a0
	lla	a0,.LANCHOR1+48
	sd	s0,16(sp)
	sd	ra,24(sp)
.LCFI190:
	lla	s0,.LANCHOR1
	call	gettimeofday@plt
	ld	a5,32(s0)
	ld	a3,48(s0)
	li	a2,999424
	addiw	a2,a2,576
	sub	a3,a3,a5
	lw	t4,24(s0)
	mulw	a3,a3,a2
	lla	t1,_sysy_us
	slli	a4,t4,2
	add	t1,t1,a4
	ld	a0,40(s0)
	lw	a5,0(t1)
	ld	a1,56(s0)
	lla	a6,_sysy_s
	subw	a5,a5,a0
	addw	a5,a5,a1
	addw	a5,a5,a3
	divw	a3,a5,a2
	add	a6,a6,a4
	lw	t0,0(a6)
	lla	a1,_sysy_m
	lla	t3,_sysy_h
	lla	t5,_sysy_l2
	li	a7,60
	add	a1,a1,a4
	add	t3,t3,a4
	add	a4,t5,a4
	sw	s1,0(a4)
	lw	a0,0(a1)
	lw	t6,0(t3)
	addiw	t4,t4,1
	ld	ra,24(sp)
.LCFI191:
	sw	t4,24(s0)
	ld	s0,16(sp)
.LCFI192:
	ld	s1,8(sp)
.LCFI193:
	addw	a3,a3,t0
	divw	a4,a3,a7
	addw	a4,a4,a0
	divw	a0,a4,a7
	remw	a5,a5,a2
	addw	a2,a0,t6
	sw	a2,0(t3)
	remw	a3,a3,a7
	sw	a5,0(t1)
	remw	a4,a4,a7
	sw	a3,0(a6)
	sw	a4,0(a1)
	addi	sp,sp,32
.LCFI194:
	jr	ra
.LFE66:
	.size	_sysy_stoptime, .-_sysy_stoptime
	.globl	_sysy_idx
	.globl	_sysy_us
	.globl	_sysy_s
	.globl	_sysy_m
	.globl	_sysy_h
	.globl	_sysy_l2
	.globl	_sysy_l1
	.globl	_sysy_end
	.globl	_sysy_start
	.section	.rodata.cst8,"aM",@progbits,8
	.align	3
.LC0:
	.word	0
	.word	1072693248
	.section	.rodata.cst4,"aM",@progbits,4
	.align	2
.LC1:
	.word	-2147483648
	.section	.rodata.cst8
	.align	3
.LC2:
	.word	0
	.word	1076101120
	.section	.rodata
	.align	3
	.set	.LANCHOR2,. + 0
	.type	hex_digits, @object
	.size	hex_digits, 17
hex_digits:
	.string	"0123456789abcdef"
	.zero	7
	.type	dec2_tbl, @object
	.size	dec2_tbl, 201
dec2_tbl:
	.string	"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899"
	.data
	.align	2
	.set	.LANCHOR0,. + 0
	.type	in_unget, @object
	.size	in_unget, 4
in_unget:
	.word	-1
	.bss
	.align	3
	.set	.LANCHOR1,. + 0
	.type	in_pos, @object
	.size	in_pos, 8
in_pos:
	.zero	8
	.type	in_len, @object
	.size	in_len, 8
in_len:
	.zero	8
	.type	out_len, @object
	.size	out_len, 8
out_len:
	.zero	8
	.type	_sysy_idx, @object
	.size	_sysy_idx, 4
_sysy_idx:
	.zero	4
	.zero	4
	.type	_sysy_start, @object
	.size	_sysy_start, 16
_sysy_start:
	.zero	16
	.type	_sysy_end, @object
	.size	_sysy_end, 16
_sysy_end:
	.zero	16
	.type	in_buf, @object
	.size	in_buf, 8192
in_buf:
	.zero	8192
	.type	out_buf, @object
	.size	out_buf, 8192
out_buf:
	.zero	8192
	.type	_sysy_us, @object
	.size	_sysy_us, 4096
_sysy_us:
	.zero	4096
	.type	_sysy_s, @object
	.size	_sysy_s, 4096
_sysy_s:
	.zero	4096
	.type	_sysy_m, @object
	.size	_sysy_m, 4096
_sysy_m:
	.zero	4096
	.type	_sysy_h, @object
	.size	_sysy_h, 4096
_sysy_h:
	.zero	4096
	.type	_sysy_l2, @object
	.size	_sysy_l2, 4096
_sysy_l2:
	.zero	4096
	.type	_sysy_l1, @object
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
	.sleb128 -4
	.uleb128 0x1
	.uleb128 0x1
	.byte	0x1b
	.byte	0xc
	.uleb128 0x2
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
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI1-.LCFI0
	.byte	0x88
	.uleb128 0x4
	.byte	0x4
	.4byte	.LCFI2-.LCFI1
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI3-.LCFI2
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI4-.LCFI3
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI5-.LCFI4
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI6-.LCFI5
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
	.4byte	.LCFI7-.LFB41
	.byte	0xe
	.uleb128 0x30
	.byte	0x4
	.4byte	.LCFI8-.LCFI7
	.byte	0x93
	.uleb128 0xa
	.byte	0x4
	.4byte	.LCFI9-.LCFI8
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x92
	.uleb128 0x8
	.byte	0x94
	.uleb128 0xc
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI10-.LCFI9
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI11-.LCFI10
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI12-.LCFI11
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI13-.LCFI12
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI14-.LCFI13
	.byte	0xd3
	.byte	0x4
	.4byte	.LCFI15-.LCFI14
	.byte	0xd4
	.byte	0x4
	.4byte	.LCFI16-.LCFI15
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI17-.LCFI16
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI18-.LCFI17
	.byte	0xe
	.uleb128 0
	.byte	0xc1
	.byte	0xc8
	.byte	0xc9
	.byte	0xd2
	.byte	0xd3
	.byte	0xd4
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
	.4byte	.LCFI19-.LFB40
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI20-.LCFI19
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI21-.LCFI20
	.byte	0x88
	.uleb128 0x4
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI22-.LCFI21
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI23-.LCFI22
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI24-.LCFI23
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI25-.LCFI24
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI26-.LCFI25
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
	.4byte	.LCFI27-.LFB50
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI28-.LCFI27
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x81
	.uleb128 0x2
	.byte	0x92
	.uleb128 0x8
	.byte	0x4
	.4byte	.LCFI29-.LCFI28
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI30-.LCFI29
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI31-.LCFI30
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI32-.LCFI31
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI33-.LCFI32
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI34-.LCFI33
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
	.4byte	.LCFI35-.LFB51
	.byte	0xe
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI36-.LCFI35
	.byte	0x88
	.uleb128 0x4
	.byte	0x4
	.4byte	.LCFI37-.LCFI36
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI38-.LCFI37
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI39-.LCFI38
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI40-.LCFI39
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI41-.LCFI40
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
	.4byte	.LCFI42-.LFB54
	.byte	0xe
	.uleb128 0xa0
	.byte	0x4
	.4byte	.LCFI43-.LCFI42
	.byte	0x92
	.uleb128 0x8
	.byte	0x4
	.4byte	.LCFI44-.LCFI43
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x81
	.uleb128 0x2
	.byte	0x93
	.uleb128 0xa
	.byte	0x94
	.uleb128 0xc
	.byte	0x4
	.4byte	.LCFI45-.LCFI44
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI46-.LCFI45
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI47-.LCFI46
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI48-.LCFI47
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI49-.LCFI48
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI50-.LCFI49
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI51-.LCFI50
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI52-.LCFI51
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI53-.LCFI52
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI54-.LCFI53
	.byte	0xd3
	.byte	0x4
	.4byte	.LCFI55-.LCFI54
	.byte	0xd4
	.byte	0x4
	.4byte	.LCFI56-.LCFI55
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI57-.LCFI56
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI58-.LCFI57
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI59-.LCFI58
	.byte	0xd5
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI60-.LCFI59
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI61-.LCFI60
	.byte	0xd5
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI62-.LCFI61
	.byte	0xa
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI63-.LCFI62
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI64-.LCFI63
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI65-.LCFI64
	.byte	0xd5
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI66-.LCFI65
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI67-.LCFI66
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI68-.LCFI67
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI69-.LCFI68
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI70-.LCFI69
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI71-.LCFI70
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI72-.LCFI71
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI73-.LCFI72
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI74-.LCFI73
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI75-.LCFI74
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI76-.LCFI75
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI77-.LCFI76
	.byte	0xa
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI78-.LCFI77
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI79-.LCFI78
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI80-.LCFI79
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI81-.LCFI80
	.byte	0xd5
	.byte	0xd6
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI82-.LCFI81
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI83-.LCFI82
	.byte	0xa
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI84-.LCFI83
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI85-.LCFI84
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI86-.LCFI85
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
	.4byte	.LCFI87-.LFB55
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI88-.LCFI87
	.byte	0x88
	.uleb128 0x4
	.byte	0x92
	.uleb128 0x8
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI89-.LCFI88
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI90-.LCFI89
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI91-.LCFI90
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI92-.LCFI91
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI93-.LCFI92
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI94-.LCFI93
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
	.4byte	.LCFI95-.LFB56
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI96-.LCFI95
	.byte	0x88
	.uleb128 0x4
	.byte	0x92
	.uleb128 0x8
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI97-.LCFI96
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI98-.LCFI97
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI99-.LCFI98
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI100-.LCFI99
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI101-.LCFI100
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI102-.LCFI101
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
	.4byte	.LCFI103-.LFB57
	.byte	0xe
	.uleb128 0x40
	.byte	0x4
	.4byte	.LCFI104-.LCFI103
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI105-.LCFI104
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI106-.LCFI105
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI107-.LCFI106
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI108-.LCFI107
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI109-.LCFI108
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI110-.LCFI109
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
	.4byte	.LCFI111-.LFB58
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI112-.LCFI111
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI113-.LCFI112
	.byte	0x88
	.uleb128 0x4
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI114-.LCFI113
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI115-.LCFI114
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI116-.LCFI115
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI117-.LCFI116
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI118-.LCFI117
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
	.4byte	.LCFI119-.LFB59
	.byte	0xe
	.uleb128 0x70
	.byte	0x4
	.4byte	.LCFI120-.LCFI119
	.byte	0x89
	.uleb128 0x6
	.byte	0x94
	.uleb128 0xc
	.byte	0x4
	.4byte	.LCFI121-.LCFI120
	.byte	0x88
	.uleb128 0x4
	.byte	0x93
	.uleb128 0xa
	.byte	0x4
	.4byte	.LCFI122-.LCFI121
	.byte	0x81
	.uleb128 0x2
	.byte	0x92
	.uleb128 0x8
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI123-.LCFI122
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x4
	.4byte	.LCFI124-.LCFI123
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI125-.LCFI124
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI126-.LCFI125
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI127-.LCFI126
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI128-.LCFI127
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI129-.LCFI128
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI130-.LCFI129
	.byte	0xd3
	.byte	0x4
	.4byte	.LCFI131-.LCFI130
	.byte	0xd4
	.byte	0x4
	.4byte	.LCFI132-.LCFI131
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI133-.LCFI132
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI134-.LCFI133
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI135-.LCFI134
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
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
	.4byte	.LCFI136-.LFB60
	.byte	0xe
	.uleb128 0x50
	.byte	0x4
	.4byte	.LCFI137-.LCFI136
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI138-.LCFI137
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI139-.LCFI138
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI140-.LCFI139
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI141-.LCFI140
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI142-.LCFI141
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI143-.LCFI142
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
	.4byte	.LCFI144-.LFB61
	.byte	0xe
	.uleb128 0x90
	.byte	0x4
	.4byte	.LCFI145-.LCFI144
	.byte	0x89
	.uleb128 0x6
	.byte	0x94
	.uleb128 0xc
	.byte	0x4
	.4byte	.LCFI146-.LCFI145
	.byte	0x88
	.uleb128 0x4
	.byte	0x92
	.uleb128 0x8
	.byte	0x4
	.4byte	.LCFI147-.LCFI146
	.byte	0x81
	.uleb128 0x2
	.byte	0x93
	.uleb128 0xa
	.byte	0x4
	.4byte	.LCFI148-.LCFI147
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x97
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI149-.LCFI148
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI150-.LCFI149
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI151-.LCFI150
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI152-.LCFI151
	.byte	0xa
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI153-.LCFI152
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI154-.LCFI153
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI155-.LCFI154
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI156-.LCFI155
	.byte	0xd3
	.byte	0x4
	.4byte	.LCFI157-.LCFI156
	.byte	0xd4
	.byte	0x4
	.4byte	.LCFI158-.LCFI157
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI159-.LCFI158
	.byte	0xb
	.byte	0x4
	.4byte	.LCFI160-.LCFI159
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x97
	.uleb128 0x12
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
	.4byte	.LCFI161-.LFB62
	.byte	0xe
	.uleb128 0x70
	.byte	0x4
	.4byte	.LCFI162-.LCFI161
	.byte	0x89
	.uleb128 0x16
	.byte	0x4
	.4byte	.LCFI163-.LCFI162
	.byte	0x88
	.uleb128 0x14
	.byte	0x92
	.uleb128 0x18
	.byte	0x81
	.uleb128 0x12
	.byte	0x4
	.4byte	.LCFI164-.LCFI163
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI165-.LCFI164
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI166-.LCFI165
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI167-.LCFI166
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI168-.LCFI167
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI169-.LCFI168
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
	.4byte	.LCFI170-.LFB64
	.byte	0xe
	.uleb128 0x90
	.byte	0x4
	.4byte	.LCFI171-.LCFI170
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI172-.LCFI171
	.byte	0x88
	.uleb128 0x4
	.byte	0x89
	.uleb128 0x6
	.byte	0x92
	.uleb128 0x8
	.byte	0x93
	.uleb128 0xa
	.byte	0x94
	.uleb128 0xc
	.byte	0x95
	.uleb128 0xe
	.byte	0x96
	.uleb128 0x10
	.byte	0x97
	.uleb128 0x12
	.byte	0x98
	.uleb128 0x14
	.byte	0x99
	.uleb128 0x16
	.byte	0x9a
	.uleb128 0x18
	.byte	0x9b
	.uleb128 0x1a
	.byte	0x4
	.4byte	.LCFI173-.LCFI172
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI174-.LCFI173
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI175-.LCFI174
	.byte	0xd2
	.byte	0x4
	.4byte	.LCFI176-.LCFI175
	.byte	0xd3
	.byte	0x4
	.4byte	.LCFI177-.LCFI176
	.byte	0xd4
	.byte	0x4
	.4byte	.LCFI178-.LCFI177
	.byte	0xd5
	.byte	0x4
	.4byte	.LCFI179-.LCFI178
	.byte	0xd6
	.byte	0x4
	.4byte	.LCFI180-.LCFI179
	.byte	0xd7
	.byte	0x4
	.4byte	.LCFI181-.LCFI180
	.byte	0xd8
	.byte	0x4
	.4byte	.LCFI182-.LCFI181
	.byte	0xd9
	.byte	0x4
	.4byte	.LCFI183-.LCFI182
	.byte	0xda
	.byte	0x4
	.4byte	.LCFI184-.LCFI183
	.byte	0xdb
	.byte	0x4
	.4byte	.LCFI185-.LCFI184
	.byte	0xa
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI186-.LCFI185
	.byte	0xe
	.uleb128 0
	.byte	0x4
	.4byte	.LCFI187-.LCFI186
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
	.4byte	.LCFI188-.LFB66
	.byte	0xe
	.uleb128 0x20
	.byte	0x4
	.4byte	.LCFI189-.LCFI188
	.byte	0x89
	.uleb128 0x6
	.byte	0x4
	.4byte	.LCFI190-.LCFI189
	.byte	0x88
	.uleb128 0x4
	.byte	0x81
	.uleb128 0x2
	.byte	0x4
	.4byte	.LCFI191-.LCFI190
	.byte	0xc1
	.byte	0x4
	.4byte	.LCFI192-.LCFI191
	.byte	0xc8
	.byte	0x4
	.4byte	.LCFI193-.LCFI192
	.byte	0xc9
	.byte	0x4
	.4byte	.LCFI194-.LCFI193
	.byte	0xe
	.uleb128 0
	.align	3
.LEFDE39:
	.hidden	strtof
	.ident	"GCC: (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0"
	.section	.note.GNU-stack,"",@progbits
