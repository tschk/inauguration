ENTRY(Reset_Handler)
MEMORY { FLASH (rx) : ORIGIN = 0x10000000, LENGTH = 512K
RAM (rwx) : ORIGIN = 0x38000000, LENGTH = 256K }
SECTIONS { .text : { KEEP(*(.vector_table)) *(.text*) *(.rodata*) } > FLASH
.data : { *(.data*) } > RAM AT > FLASH
.bss : { *(.bss*) *(COMMON) } > RAM }
