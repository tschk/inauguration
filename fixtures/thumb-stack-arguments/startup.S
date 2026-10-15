.syntax unified
.cpu cortex-m33
.thumb
.section .vector_table,"a",%progbits
.word 0x38040000
.word Reset_Handler
.section .text,"ax",%progbits
.global Reset_Handler
.thumb_func
Reset_Handler:
  ldr r4, =0x40200000
  movs r0, #16
  str r0, [r4, #16]
  movs r0, #1
  str r0, [r4, #8]
  bl main
  cmp r0, #42
  ite eq
  ldreq r5, =passed
  ldrne r5, =failed
1:
  ldrb r0, [r5], #1
  cbz r0, 3f
2:
  ldr r1, [r4, #4]
  tst r1, #1
  bne 2b
  str r0, [r4]
  b 1b
3:
  wfi
  b 3b
.global stack_alignment
.thumb_func
stack_alignment:
  mov r0, sp
  movs r1, #7
  ands r0, r1
  bx lr
passed: .asciz "PASS stack arguments\n"
failed: .asciz "FAIL stack arguments\n"
