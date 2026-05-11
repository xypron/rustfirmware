//! Host-side formatter validation binary.
//!
//! This tool replaces the firmware `sbi::puts()` backend with a local
//! `std::print!`-backed implementation so the shared formatter in
//! `src/print.rs` can be exercised directly on the host.

use std::error::Error;
use std::io;
use std::sync::{Mutex, OnceLock};

/// Host-side `sbi` shim used by the shared formatter module.
mod sbi {
    use super::{Mutex, OnceLock};

    /// Captured text emitted through the host-side `puts()` shim.
    static OUTPUT: OnceLock<Mutex<String>> = OnceLock::new();

    /// Appends one formatted fragment to stdout and the capture buffer.
    ///
    /// # Parameters
    ///
    /// - `message`: Text emitted by the shared formatter.
    pub(crate) fn puts(message: &str) {
        std::print!("{}", message);

        let output = OUTPUT.get_or_init(|| Mutex::new(String::new()));
        let mut buffer = output.lock().unwrap();
        buffer.push_str(message);
    }

    /// Clears the captured formatter output.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub(crate) fn clear_output() {
        let output = OUTPUT.get_or_init(|| Mutex::new(String::new()));
        output.lock().unwrap().clear();
    }

    /// Returns the captured formatter output accumulated so far.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub(crate) fn captured_output() -> String {
        let output = OUTPUT.get_or_init(|| Mutex::new(String::new()));
        output.lock().unwrap().clone()
    }
}

#[allow(dead_code)]
#[path = "../print.rs"]
mod print;

/// One EFI memory type string used to validate the formatter.
const EFI_RESERVED_TYPE_NAME: &str = "EfiReservedMemoryType";

/// Runs the host-side formatter checks.
///
/// # Parameters
///
/// This function does not accept parameters.
fn main() -> Result<(), Box<dyn Error>> {
    verify_basic_string_formatting()?;
    verify_partition_style_formatting()?;
    verify_memory_map_type_formatting()?;
    verify_helper_returned_type_formatting()?;

    std::println!("print_test: formatter output matches expectations");
    Ok(())
}

/// Verifies that one plain string argument renders through `{}`.
///
/// # Parameters
///
/// This function does not accept parameters.
fn verify_basic_string_formatting() -> Result<(), Box<dyn Error>> {
    sbi::clear_output();
    crate::println!("greeting={}", "hello");
    expect_output("greeting=hello\n")
}

/// Verifies one mixed formatting call similar to normal boot diagnostics.
///
/// # Parameters
///
/// This function does not accept parameters.
fn verify_partition_style_formatting() -> Result<(), Box<dyn Error>> {
    sbi::clear_output();
    crate::println!(
        "partition {}: start={}, size={}, label='{}', type='{}', fs='{}', bootflag={}",
        1usize,
        235520u64,
        33318879u64,
        "-",
        "Linux filesystem",
        "ext4",
        true,
    );

    expect_output(
        "partition 1: start=235520, size=33318879, label='-', type='Linux filesystem', fs='ext4', bootflag=true\n",
    )
}

/// Verifies the exact EFI memory-map line that regressed during boot logging.
///
/// # Parameters
///
/// This function does not accept parameters.
fn verify_memory_map_type_formatting() -> Result<(), Box<dyn Error>> {
    sbi::clear_output();
    crate::println!(
        "efi-memory {}: type={}, base={:#018x}, size={:#018x}, attr={:#018x}",
        1usize,
        EFI_RESERVED_TYPE_NAME,
        0x8000_0000usize,
        0x0006_0000usize,
        0u64,
    );

    expect_output(
        "efi-memory 1: type=EfiReservedMemoryType, base=0x0000000080000000, size=0x0000000000060000, attr=0x0000000000000000\n",
    )
}

/// Verifies the same formatter path using a helper-returned string.
///
/// # Parameters
///
/// This function does not accept parameters.
fn verify_helper_returned_type_formatting() -> Result<(), Box<dyn Error>> {
    sbi::clear_output();

    let memory_type_name: &'static str = efi_memory_type_name(0);
    crate::println!(
        "efi-memory {}: type={}, base={:#018x}, size={:#018x}, attr={:#018x}",
        1usize,
        memory_type_name,
        0x8000_0000usize,
        0x0006_0000usize,
        0u64,
    );

    expect_output(
        "efi-memory 1: type=EfiReservedMemoryType, base=0x0000000080000000, size=0x0000000000060000, attr=0x0000000000000000\n",
    )
}

/// Returns a short test string for one EFI memory type code.
///
/// # Parameters
///
/// - `memory_type`: EFI memory type encoded as a raw descriptor value.
fn efi_memory_type_name(memory_type: u32) -> &'static str {
    match memory_type {
        0 => EFI_RESERVED_TYPE_NAME,
        _ => "unknown",
    }
}

/// Compares the captured formatter output with one expected string.
///
/// # Parameters
///
/// - `expected`: Exact formatter output expected from the previous check.
fn expect_output(expected: &str) -> Result<(), Box<dyn Error>> {
    let actual = sbi::captured_output();
    if actual == expected {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "formatter output mismatch\nexpected: {expected:?}\nactual:   {actual:?}",
    ))
    .into())
}