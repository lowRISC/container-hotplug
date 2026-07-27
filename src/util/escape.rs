//! Undoing udev's escaping of device strings.

/// The input was not validly udev-escaped.
#[derive(thiserror::Error, Debug)]
#[error("Invalid escape sequence")]
pub struct InvalidEscapeError;

/// Unescape a udev-escaped devnode name.
///
/// udev replaces characters that are unsafe in a device name with `\xNN` hex escapes, e.g. in the
/// `ID_VENDOR_ENC` and `ID_MODEL_ENC` properties, where it escapes spaces. Each escaped byte becomes
/// the character with that value, i.e. it is decoded as Latin-1 rather than as part of a UTF-8
/// sequence; good enough for the display names this is used for.
pub fn unescape_devnode(escaped: &str) -> Result<String, InvalidEscapeError> {
    let mut result = String::with_capacity(escaped.len());
    let mut iter = escaped.chars();

    while let Some(c) = iter.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }

        // Udev escaped devnode names only use hexadecimal escapes, so we only need to handle those.
        if iter.next() != Some('x') {
            return Err(InvalidEscapeError);
        }

        let hex = iter.as_str().get(..2).ok_or(InvalidEscapeError)?;
        result.push(
            u8::from_str_radix(hex, 16)
                .map_err(|_| InvalidEscapeError)?
                .into(),
        );
        iter.next();
        iter.next();
    }

    Ok(result)
}
