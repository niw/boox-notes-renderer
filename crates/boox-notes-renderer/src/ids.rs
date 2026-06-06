//! UUID normalization.
//!
//! Boox mixes two textual UUID forms within a single note: the "simple" 32 hex
//! form (`03b2164818834f23839fb3d803780d9c`) and the "hyphenated" 36 form
//! (`03b21648-1883-4f23-839f-b3d803780d9c`). Directory and file names use
//! either, and binary headers pad UUIDs with trailing spaces/nulls. We normalize
//! everything to the simple form for keying.

/// Strip hyphens, whitespace and NUL padding, lowercasing the result — both
/// UUID forms map to the same 32-char simple form. Non-UUID input gets the
/// same stripping (no UUID validation); that's fine for keying because every
/// key on both sides of a lookup goes through this same function.
pub fn normalize(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '\0' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Decode a fixed-width UUID field from a binary header (space/NUL padded).
pub fn from_padded_bytes(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    normalize(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_both_forms_equally() {
        let simple = "03b2164818834f23839fb3d803780d9c";
        let hyphen = "03b21648-1883-4f23-839f-b3d803780d9c";
        assert_eq!(normalize(simple), simple);
        assert_eq!(normalize(hyphen), simple);
    }

    #[test]
    fn trims_padding() {
        assert_eq!(
            from_padded_bytes(b"03b2164818834f23839fb3d803780d9c    "),
            "03b2164818834f23839fb3d803780d9c"
        );
    }
}
