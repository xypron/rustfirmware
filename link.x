OUTPUT_ARCH(riscv)
ENTRY(_start)

MEMORY
{
    RAM (rwx) : ORIGIN = 0x0, LENGTH = 256K
}

__stack_size = 16K + 128K;

SECTIONS
{
    . = ORIGIN(RAM);
    __firmware_code_start = .;

    .text : ALIGN(4)
    {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } > RAM

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } > RAM

    __firmware_code_end = .;
    __firmware_data_start = .;

    .data : ALIGN(16)
    {
        *(.data .data.*)
    } > RAM

    .bss (NOLOAD) : ALIGN(16)
    {
        *(.bss .bss.*)
        *(COMMON)
    } > RAM

    . = ALIGN(16);
    __firmware_data_end = .;
    __heap_start = .;
    __heap_end = ORIGIN(RAM) + LENGTH(RAM);
    __stack_top = ORIGIN(RAM) + LENGTH(RAM);
    __stack_bottom = __stack_top - __stack_size;

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.comment*)
    }
}