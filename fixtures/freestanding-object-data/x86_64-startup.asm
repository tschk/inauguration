; Generic Multiboot entry: loader enters 32-bit protected mode, then identity
; maps the first GiB and enters 64-bit mode before calling the linked object.
BITS 32
section .multiboot align=4
 dd 0x1badb002,0,-0x1badb002
section .text.boot
global _start
extern main
_start:
 cli
 mov esp,0x90000
 cld
 mov edi,0x1000
 xor eax,eax
 mov ecx,0x3000/4
 rep stosd
 mov dword [0x1000],0x2003
 mov dword [0x2000],0x3003
 mov edi,0x3000
 mov eax,0x83
 mov ecx,512
.pages:
 mov [edi],eax
 add eax,0x200000
 add edi,8
 loop .pages
 mov eax,cr4
 or eax,1<<5
 mov cr4,eax
 mov eax,0x1000
 mov cr3,eax
 mov ecx,0xc0000080
 rdmsr
 or eax,1<<8
 wrmsr
 lgdt [gdtr]
 mov eax,cr0
 or eax,0x80000000
 mov cr0,eax
 jmp 8:long_mode
BITS 64
long_mode:
 mov ax,16
 mov ds,ax
 mov es,ax
 mov ss,ax
 mov rsp,0x90000
 call main
 cmp eax,42
 lea rsi,[rel failed]
 jne .print
 lea rsi,[rel passed]
.print:
 lodsb
 test al,al
 jz .done
 mov dx,0x3f8
 out dx,al
 jmp .print
.done:
 mov dx,0xf4
 xor eax,eax
 out dx,eax
 hlt
 jmp .done
global external_answer
external_answer:
 mov eax,42
 ret
section .rodata
align 8
gdt: dq 0,0x00209a0000000000,0x0000920000000000
gdtr: dw 23
 dd gdt
passed: db 'PASS object data',10,0
failed: db 'FAIL object data',10,0
