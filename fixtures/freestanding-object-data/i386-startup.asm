BITS 32
section .multiboot align=4
  dd 0x1BADB002
  dd 0
  dd -(0x1BADB002)
section .text
global _start
extern main
_start:
  cli
  mov esp, 0x90000
  ; Initialize COM1 115200 8N1, FIFO enabled.
  mov dx, 0x3f9
  xor al, al
  out dx, al
  mov dx, 0x3fb
  mov al, 0x80
  out dx, al
  mov dx, 0x3f8
  mov al, 1
  out dx, al
  mov dx, 0x3f9
  xor al, al
  out dx, al
  mov dx, 0x3fb
  mov al, 3
  out dx, al
  mov dx, 0x3fa
  mov al, 0xc7
  out dx, al
  call main
  cmp eax, 42
  mov esi, failed
  jne .print
  mov esi, passed
.print:
  lodsb
  test al, al
  jz .done
  mov bl, al
.wait:
  mov dx, 0x3fd
  in al, dx
  test al, 0x20
  jz .wait
  mov dx, 0x3f8
  mov al, bl
  out dx, al
  jmp .print
.done:
  mov dx, 0xf4
  xor eax, eax
  out dx, eax
  hlt
  jmp .done
section .rodata
passed: db 'PASS object data',10,0
failed: db 'FAIL object data',10,0

section .text
global external_answer
external_answer:
 mov eax,42
 ret
