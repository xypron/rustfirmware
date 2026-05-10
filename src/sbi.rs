//! OpenSBI constants and environment call helpers.
//!
//! This module wraps the SBI calls used by the firmware for debug-console
//! output and system reset handling.

use core::arch::asm;

/// SBI extension ID for the debug console extension.
const SBI_EXT_DBCN: usize = 0x4442_434e;
/// SBI function ID for buffered debug console writes.
const SBI_DBCN_CONSOLE_WRITE: usize = 0;
/// SBI extension ID for the system reset extension.
const SBI_EXT_SRST: usize = 0x5352_5354;
/// SBI function ID for system reset requests.
const SBI_SRST_SYSTEM_RESET: usize = 0;
/// SRST reset type used to power off the machine.
const SBI_SRST_RESET_TYPE_SHUTDOWN: usize = 0;
/// SRST reset reason for a normal shutdown.
const SBI_SRST_RESET_REASON_NONE: usize = 0;
/// SRST reset reason for a firmware-detected failure.
const SBI_SRST_RESET_REASON_SYSTEM_FAILURE: usize = 1;

/// SBI return pair carrying an error code and one return value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SbiRet {
    /// SBI error code returned in register `a0`.
    pub(crate) error: usize,
    /// SBI result value returned in register `a1`.
    pub(crate) value: usize,
}

/// Performs one SBI environment call with the provided register arguments.
///
/// # Parameters
///
/// - `arg0`: Value loaded into register `a0` before the call.
/// - `arg1`: Value loaded into register `a1` before the call.
/// - `arg2`: Value loaded into register `a2` before the call.
/// - `arg3`: Value loaded into register `a3` before the call.
/// - `arg4`: Value loaded into register `a4` before the call.
/// - `arg5`: Value loaded into register `a5` before the call.
/// - `fid`: SBI function identifier loaded into register `a6`.
/// - `eid`: SBI extension identifier loaded into register `a7`.
fn ecall(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    fid: usize,
    eid: usize,
) -> SbiRet {
    let error;
    let value;

    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg0 => error,
            inlateout("a1") arg1 => value,
            in("a2") arg2,
            in("a3") arg3,
            in("a4") arg4,
            in("a5") arg5,
            in("a6") fid,
            in("a7") eid,
            options(nostack)
        );
    }

    SbiRet { error, value }
}

/// Writes one string slice through the SBI debug console extension.
///
/// # Parameters
///
/// - `message`: Text buffer passed to the SBI DBCN console write call.
pub(crate) fn puts(message: &str) -> SbiRet {
    ecall(
        message.len(),
        message.as_ptr() as usize,
        0,
        0,
        0,
        0,
        SBI_DBCN_CONSOLE_WRITE,
        SBI_EXT_DBCN,
    )
}

/// Stops forward progress by repeatedly waiting for interrupts.
///
/// # Parameters
///
/// This function does not accept parameters.
fn halt() -> ! {
    loop {
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Requests an SBI system reset and halts if the call returns.
///
/// # Parameters
///
/// - `reset_type`: SBI reset type passed to the SRST extension.
/// - `reset_reason`: SBI reset reason passed to the SRST extension.
fn system_reset(reset_type: usize, reset_reason: usize) -> ! {
    let _ = ecall(
        reset_type,
        reset_reason,
        0,
        0,
        0,
        0,
        SBI_SRST_SYSTEM_RESET,
        SBI_EXT_SRST,
    );

    halt()
}

/// Powers off the machine through the SBI system reset extension.
///
/// # Parameters
///
/// This function does not accept parameters.
pub(crate) fn poweroff() -> ! {
    system_reset(SBI_SRST_RESET_TYPE_SHUTDOWN, SBI_SRST_RESET_REASON_NONE)
}

/// Powers off the machine and reports a firmware-detected failure.
///
/// # Parameters
///
/// This function does not accept parameters.
pub(crate) fn poweroff_on_failure() -> ! {
    system_reset(
        SBI_SRST_RESET_TYPE_SHUTDOWN,
        SBI_SRST_RESET_REASON_SYSTEM_FAILURE,
    )
}