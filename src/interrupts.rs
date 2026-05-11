//! S-mode trap vector setup and trap handling.
//!
//! This module installs the firmware trap vector into `stvec` and handles
//! synchronous exceptions or interrupts that occur while the firmware is
//! executing in supervisor mode.

use core::arch::{asm, global_asm};

use crate::sbi::poweroff_on_failure;

unsafe extern "C" {
    /// Assembly trap entry installed into the S-mode trap vector register.
    static smode_trap_vector: u8;
}

global_asm!(
    r#"
    .balign 4
    .globl smode_trap_vector
smode_trap_vector:
    andi sp, sp, -16
    csrr a0, scause
    csrr a1, stval
    csrr a2, sepc
    tail rust_smode_trap
"#
);

/// Returns the linked image offset of the S-mode trap vector.
///
/// The caller adds this image-relative offset to the active runtime base
/// address before writing the relocated trap entry into `stvec`.
///
/// # Parameters
///
/// This function does not accept parameters.
pub(crate) fn smode_trap_vector_offset() -> usize {
    core::ptr::addr_of!(smode_trap_vector) as usize
}

/// Installs the direct S-mode trap vector used for firmware exceptions.
///
/// # Parameters
///
/// - `vector_address`: Runtime address of the trap entry to write into `stvec`.
///   The address must be 4-byte aligned so the `stvec` MODE bits remain `0`
///   and the CPU stays in direct trap-vector mode.
pub(crate) fn install_smode_trap_vector(vector_address: usize) {
    debug_assert_eq!(vector_address & 0x3, 0);

    unsafe {
        asm!(
            "csrw stvec, {vector}",
            vector = in(reg) vector_address,
            options(nostack, preserves_flags)
        );
    }
}

/// Handles one S-mode trap raised while firmware code is executing.
///
/// This firmware treats every S-mode trap as fatal, reports the decoded cause,
/// and powers the machine off instead of attempting in-place recovery.
///
/// # Parameters
///
/// - `scause`: Trap cause value captured from the `scause` CSR.
/// - `stval`: Trap value captured from the `stval` CSR.
/// - `sepc`: Trap program counter captured from the `sepc` CSR.
#[unsafe(no_mangle)]
extern "C" fn rust_smode_trap(scause: usize, stval: usize, sepc: usize) -> ! {
    let interrupt_bit = 1usize << (usize::BITS - 1);
    let is_interrupt = (scause & interrupt_bit) != 0;
    let cause_code = scause & !interrupt_bit;
    let cause_name = trap_cause_name(is_interrupt, cause_code);

    if is_interrupt {
        crate::println!(
            "rustfimware: s-mode interrupt cause={} ({}) stval={:#018x} sepc={:#018x}",
            cause_code,
            cause_name,
            stval,
            sepc,
        );
    } else {
        crate::println!(
            "rustfimware: s-mode exception cause={} ({}) stval={:#018x} sepc={:#018x}",
            cause_code,
            cause_name,
            stval,
            sepc,
        );
    }

    poweroff_on_failure()
}

/// Returns one short symbolic name for a trap cause code.
///
/// # Parameters
///
/// - `is_interrupt`: Whether `cause_code` describes an interrupt.
/// - `cause_code`: Architecture-defined trap cause value without the high bit.
fn trap_cause_name(is_interrupt: bool, cause_code: usize) -> &'static str {
    if is_interrupt {
        match cause_code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "unknown interrupt",
        }
    } else {
        match cause_code {
            0 => "instruction address misaligned",
            1 => "instruction access fault",
            2 => "illegal instruction",
            3 => "breakpoint",
            4 => "load address misaligned",
            5 => "load access fault",
            6 => "store or AMO address misaligned",
            7 => "store or AMO access fault",
            8 => "environment call from U-mode",
            9 => "environment call from S-mode",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store or AMO page fault",
            _ => "unknown exception",
        }
    }
}