OUTPUT_ARCH(riscv)
ENTRY(_start)

MEMORY
{
    RAM (rwx) : ORIGIN = 0x80200000, LENGTH = 128K
}

SECTIONS
{
    . = ORIGIN(RAM);

    .text : ALIGN(4)
    {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } > RAM

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } > RAM

    .data : ALIGN(16)
    {
        *(.data .data.*)
    } > RAM

    .bss (NOLOAD) : ALIGN(16)
    {
        *(.bss .bss.*)
        *(COMMON)
    } > RAM

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.comment*)
    }
}