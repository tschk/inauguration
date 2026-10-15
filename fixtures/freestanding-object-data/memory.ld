ENTRY(_start)
SECTIONS {
 . = 0x100000;
 .multiboot : { KEEP(*(.multiboot)) }
 .boot : { *(.text.boot) }
 . = 0x400000;
 .text : { *(.text*) }
 .rodata : { *(.rodata*) }
 . = 0x900000;
 .data : { *(.data*) }
 .bss : { *(.bss*) *(COMMON) }
}
