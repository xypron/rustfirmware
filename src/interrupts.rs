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
    csrr a0, scause
    csrr a1, stval
    csrr a2, sepc
    tail rust_smode_trap
"#
);

/// Returns the linked offset of the S-mode trap vector inside the image.
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
pub(crate) fn install_smode_trap_vector(vector_address: usize) {
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

    if is_interrupt {
        crate::println!(
            "rustfimware: s-mode interrupt cause={} stval={:#018x} sepc={:#018x}",
            cause_code,
            stval,
            sepc,
        );
    } else {
        crate::println!(
            "rustfimware: s-mode exception cause={} stval={:#018x} sepc={:#018x}",
            cause_code,
            stval,
            sepc,
        );
    }

    poweroff_on_failure()
}