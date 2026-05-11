//! S-mode trap vector setup and trap handling.
//!
//! This module installs the firmware trap vector into `stvec` and handles
//! synchronous exceptions or interrupts that occur while the firmware is
//! executing in supervisor mode.

use core::arch::{asm, global_asm};

use crate::sbi::{poweroff_on_failure, puts};

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

/// Returns the runtime address of the active image trap vector.
///
/// # Parameters
///
/// - `runtime_base`: Runtime base address of the active firmware image.
pub(crate) fn trap_vector_address(runtime_base: usize) -> usize {
    runtime_base
        .checked_add(smode_trap_vector_offset())
        .unwrap()
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

/// Triggers one deliberate illegal-instruction trap for diagnostics testing.
///
/// This helper is intended for temporary local debugging after the S-mode trap
/// vector has been installed. Do not leave active calls to it in normal boot
/// paths.
///
/// # Parameters
///
/// This function does not accept parameters.
#[allow(dead_code)]
pub(crate) fn trigger_invalid_instruction_trap() {
    crate::println!("rustfimware: triggering test illegal instruction");

    unsafe {
        asm!(".4byte 0");
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

    if is_interrupt {
        crate::print!("rustfimware: s-mode interrupt cause={} (", cause_code);
    } else {
        crate::print!("rustfimware: s-mode exception cause={} (", cause_code);
    }

    print_trap_cause_name(is_interrupt, cause_code);
    crate::println!(") stval={:#018x} sepc={:#018x}", stval, sepc);

    poweroff_on_failure()
}

/// Prints one short symbolic name for a trap cause code.
///
/// # Parameters
///
/// - `is_interrupt`: Whether `cause_code` describes an interrupt.
/// - `cause_code`: Architecture-defined trap cause value without the high bit.
fn print_trap_cause_name(is_interrupt: bool, cause_code: usize) {
    if is_interrupt {
        match cause_code {
            1 => {
                let _ = puts("supervisor software interrupt");
            }
            5 => {
                let _ = puts("supervisor timer interrupt");
            }
            9 => {
                let _ = puts("supervisor external interrupt");
            }
            _ => {
                let _ = puts("unknown interrupt");
            }
        }
    } else {
        match cause_code {
            0 => {
                let _ = puts("instruction address misaligned");
            }
            1 => {
                let _ = puts("instruction access fault");
            }
            2 => {
                let _ = puts("illegal instruction");
            }
            3 => {
                let _ = puts("breakpoint");
            }
            4 => {
                let _ = puts("load address misaligned");
            }
            5 => {
                let _ = puts("load access fault");
            }
            6 => {
                let _ = puts("store or AMO address misaligned");
            }
            7 => {
                let _ = puts("store or AMO access fault");
            }
            8 => {
                let _ = puts("environment call from U-mode");
            }
            9 => {
                let _ = puts("environment call from S-mode");
            }
            12 => {
                let _ = puts("instruction page fault");
            }
            13 => {
                let _ = puts("load page fault");
            }
            15 => {
                let _ = puts("store or AMO page fault");
            }
            _ => {
                let _ = puts("unknown exception");
            }
        }
    }
}