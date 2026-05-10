//! Formatted console output helpers.
//!
//! This module provides a small formatting engine over the OpenSBI debug
//! console backend so firmware code can use Rust-style formatting syntax
//! without depending on the full `core::fmt` runtime path.

use crate::sbi::puts;
use core::str;

/// One runtime formatting argument supported by the firmware printer.
pub(crate) enum FormatArgument<'a> {
    /// UTF-8 string slice argument.
    Str(&'a str),
    /// Boolean argument rendered as `true` or `false`.
    Bool(bool),
    /// Machine-sized unsigned integer argument.
    Usize(usize),
    /// `u64` argument.
    U64(u64),
    /// `u32` argument.
    U32(u32),
}

/// Converts a value into one runtime formatting argument.
pub(crate) trait IntoFormatArgument<'a> {
    /// Returns the runtime formatting argument for this value.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a>;
}

impl<'a> IntoFormatArgument<'a> for &'a str {
    /// Returns the string formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::Str(self)
    }
}

impl<'a> IntoFormatArgument<'a> for bool {
    /// Returns the boolean formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::Bool(self)
    }
}

impl<'a> IntoFormatArgument<'a> for usize {
    /// Returns the machine-sized integer formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::Usize(self)
    }
}

impl<'a> IntoFormatArgument<'a> for u64 {
    /// Returns the `u64` formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::U64(self)
    }
}

impl<'a> IntoFormatArgument<'a> for u32 {
    /// Returns the `u32` formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::U32(self)
    }
}

impl<'a> IntoFormatArgument<'a> for u8 {
    /// Returns the `u8` formatting argument.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn into_argument(self) -> FormatArgument<'a> {
        FormatArgument::U32(self as u32)
    }
}

/// Converts one supported value into a runtime formatting argument.
///
/// # Parameters
///
/// - `value`: Value to convert for the print macros.
pub(crate) fn into_argument<'a, T: IntoFormatArgument<'a>>(
    value: T,
) -> FormatArgument<'a> {
    value.into_argument()
}

/// Writes one format string with its argument array.
///
/// Supported forms include `{}`, `{:x}`, `{:#x}`, `{:02x}`, and
/// `{:#018x}`, plus escaped braces `{{` and `}}`.
///
/// # Parameters
///
/// - `format`: Format string literal.
/// - `arguments`: Runtime argument array consumed left to right.
pub(crate) fn print(format: &str, arguments: &[FormatArgument<'_>]) {
    let bytes = format.as_bytes();
    let mut literal_start = 0usize;
    let mut index = 0usize;
    let mut argument_index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                if index + 1 < bytes.len() && bytes[index + 1] == b'{' {
                    flush_literal(format, literal_start, index);
                    let _ = puts("{");
                    index += 2;
                    literal_start = index;
                    continue;
                }

                flush_literal(format, literal_start, index);
                let Some(spec_end) = find_closing_brace(bytes, index + 1) else {
                    let _ = puts("<format-error>");
                    return;
                };

                let spec = &format[index + 1..spec_end];
                let Some(argument) = arguments.get(argument_index) else {
                    let _ = puts("<missing-arg>");
                    return;
                };
                argument_index += 1;
                print_argument(argument, spec);

                index = spec_end + 1;
                literal_start = index;
            }
            b'}' => {
                if index + 1 < bytes.len() && bytes[index + 1] == b'}' {
                    flush_literal(format, literal_start, index);
                    let _ = puts("}");
                    index += 2;
                    literal_start = index;
                    continue;
                }

                let _ = puts("<format-error>");
                return;
            }
            _ => {
                index += 1;
            }
        }
    }

    flush_literal(format, literal_start, bytes.len());
}

/// Writes one trailing newline after the formatted output.
///
/// # Parameters
///
/// - `format`: Format string literal.
/// - `arguments`: Runtime argument array consumed left to right.
pub(crate) fn println(format: &str, arguments: &[FormatArgument<'_>]) {
    print(format, arguments);
    let _ = puts("\n");
}

/// Prints one substring from the format literal.
///
/// # Parameters
///
/// - `format`: Full format string.
/// - `start`: Inclusive byte offset of the substring start.
/// - `end`: Exclusive byte offset of the substring end.
fn flush_literal(format: &str, start: usize, end: usize) {
    if start >= end {
        return;
    }

    let _ = puts(&format[start..end]);
}

/// Returns the byte index of the next closing brace.
///
/// # Parameters
///
/// - `bytes`: Format string bytes.
/// - `start`: Byte offset where the search begins.
fn find_closing_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b'}' {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Prints one argument according to one parsed format specifier.
///
/// # Parameters
///
/// - `argument`: Argument value to print.
/// - `spec`: Format specifier text between `{` and `}`.
fn print_argument(argument: &FormatArgument<'_>, spec: &str) {
    let format_spec = parse_format_spec(spec);
    match format_spec {
        Some(specification) => match specification.kind {
            FormatKind::Display => print_display(argument),
            FormatKind::LowerHex => print_hex(argument, specification),
        },
        None => {
            let _ = puts("<format-error>");
        }
    }
}

/// One parsed format specifier.
struct ParsedFormatSpec {
    /// Rendering mode selected by the specifier.
    kind: FormatKind,
    /// Whether the alternate form should be used.
    alternate: bool,
    /// Minimum output width.
    width: usize,
    /// Whether width padding should use zero bytes.
    zero_pad: bool,
}

/// Supported formatting modes.
enum FormatKind {
    /// Standard display formatting.
    Display,
    /// Lowercase hexadecimal formatting.
    LowerHex,
}

/// Parses the text between one `{` and `}`.
///
/// # Parameters
///
/// - `spec`: Format specifier text.
fn parse_format_spec(spec: &str) -> Option<ParsedFormatSpec> {
    if spec.is_empty() {
        return Some(ParsedFormatSpec {
            kind: FormatKind::Display,
            alternate: false,
            width: 0,
            zero_pad: false,
        });
    }

    let mut chars = spec.chars();
    if chars.next()? != ':' {
        return None;
    }

    let remainder = chars.as_str();
    let mut bytes = remainder.as_bytes();
    let mut alternate = false;
    let mut zero_pad = false;
    let mut width = 0usize;

    if !bytes.is_empty() && bytes[0] == b'#' {
        alternate = true;
        bytes = &bytes[1..];
    }
    if !bytes.is_empty() && bytes[0] == b'0' {
        zero_pad = true;
        bytes = &bytes[1..];
    }

    let mut digit_index = 0usize;
    while digit_index < bytes.len() && bytes[digit_index].is_ascii_digit() {
        width = width
            .saturating_mul(10)
            .saturating_add((bytes[digit_index] - b'0') as usize);
        digit_index += 1;
    }
    bytes = &bytes[digit_index..];

    if bytes == b"x" {
        return Some(ParsedFormatSpec {
            kind: FormatKind::LowerHex,
            alternate,
            width,
            zero_pad,
        });
    }

    None
}

/// Prints one argument with display formatting.
///
/// # Parameters
///
/// - `argument`: Argument to render.
fn print_display(argument: &FormatArgument<'_>) {
    match argument {
        FormatArgument::Str(value) => {
            let _ = puts(value);
        }
        FormatArgument::Bool(value) => {
            if *value {
                let _ = puts("true");
            } else {
                let _ = puts("false");
            }
        }
        FormatArgument::Usize(value) => print_decimal_u64(*value as u64),
        FormatArgument::U64(value) => print_decimal_u64(*value),
        FormatArgument::U32(value) => print_decimal_u64(*value as u64),
    }
}

/// Prints one argument with lowercase hexadecimal formatting.
///
/// # Parameters
///
/// - `argument`: Argument to render.
/// - `specification`: Parsed hexadecimal formatting options.
fn print_hex(argument: &FormatArgument<'_>, specification: ParsedFormatSpec) {
    let value = match argument {
        FormatArgument::Usize(value) => *value as u64,
        FormatArgument::U64(value) => *value,
        FormatArgument::U32(value) => *value as u64,
        _ => {
            let _ = puts("<format-error>");
            return;
        }
    };

    print_hex_u64(
        value,
        specification.alternate,
        specification.width,
        specification.zero_pad,
    );
}

/// Emits one `u64` in decimal form.
///
/// # Parameters
///
/// - `value`: Unsigned integer that should be printed in base 10.
fn print_decimal_u64(mut value: u64) {
    let mut buffer = [0u8; 20];
    let mut index = buffer.len();

    if value == 0 {
        let _ = puts("0");
        return;
    }

    while value != 0 {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }

    let text = unsafe { str::from_utf8_unchecked(&buffer[index..]) };
    let _ = puts(text);
}

/// Emits one `u64` in lowercase hexadecimal form.
///
/// # Parameters
///
/// - `value`: Integer to print in hexadecimal.
/// - `alternate`: Whether to include the `0x` prefix.
/// - `width`: Minimum full output width.
/// - `zero_pad`: Whether to pad with leading zeroes.
fn print_hex_u64(value: u64, alternate: bool, width: usize, zero_pad: bool) {
    /// Hex digit lookup table used for manual number formatting.
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut digits = [0u8; 16];
    let mut digit_count = 0usize;
    let mut current = value;

    if current == 0 {
        digits[0] = b'0';
        digit_count = 1;
    } else {
        while current != 0 {
            digits[digit_count] = HEX_DIGITS[(current & 0xf) as usize];
            digit_count += 1;
            current >>= 4;
        }
    }

    let prefix_len = if alternate { 2 } else { 0 };
    let total_len = prefix_len + digit_count;
    let padding = width.saturating_sub(total_len);

    if !zero_pad {
        let mut index = 0usize;
        while index < padding {
            let _ = puts(" ");
            index += 1;
        }
    }

    if alternate {
        let _ = puts("0x");
    }

    if zero_pad {
        let mut index = 0usize;
        while index < padding {
            let _ = puts("0");
            index += 1;
        }
    }

    let mut buffer = [0u8; 16];
    let mut index = 0usize;
    while index < digit_count {
        buffer[index] = digits[digit_count - 1 - index];
        index += 1;
    }

    let text = unsafe { str::from_utf8_unchecked(&buffer[..digit_count]) };
    let _ = puts(text);
}

/// Writes formatted text to the OpenSBI console.
#[macro_export]
macro_rules! print {
    ($format:literal $(,)?) => {{
        $crate::print::print($format, &[]);
    }};
    ($format:literal, $($arg:expr),+ $(,)?) => {{
        let arguments = [$($crate::print::into_argument($arg)),+];
        $crate::print::print($format, &arguments);
    }};
}

/// Writes formatted text plus a trailing newline to the OpenSBI console.
#[macro_export]
macro_rules! println {
    () => {{
        $crate::print::println("", &[]);
    }};
    ($format:literal $(,)?) => {{
        $crate::print::println($format, &[]);
    }};
    ($format:literal, $($arg:expr),+ $(,)?) => {{
        let arguments = [$($crate::print::into_argument($arg)),+];
        $crate::print::println($format, &arguments);
    }};
}