//! Windows Data Protection (DPAPI) in user scope: secrets written to disk can
//! only be read back by the same Windows user (on the same machine, or with a
//! roaming profile). Pairing secrets on both sides are stored this way.

use std::io;

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

/// Extra entropy bound into every blob, so other programs' DPAPI blobs and
/// ours cannot be swapped for one another.
const ENTROPY: &[u8] = b"copilot-canvas remote secret v1";

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 }
}

/// Copies and frees a blob DPAPI allocated.
fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned `cbData` bytes at `pbData`, freed with LocalFree.
    unsafe {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as HLOCAL);
        bytes
    }
}

/// Encrypts `data` for the current Windows user.
pub fn protect(data: &[u8], description: &str) -> io::Result<Vec<u8>> {
    let description: Vec<u16> = description.encode_utf16().chain(std::iter::once(0)).collect();
    let (input, entropy) = (blob(data), blob(ENTROPY));
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: valid input blobs that outlive the call; DPAPI allocates `out`.
    let ok = unsafe { CryptProtectData(&input, description.as_ptr(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take(out))
}

/// Decrypts a blob made by [`protect`] for the current Windows user.
pub fn unprotect(sealed: &[u8]) -> io::Result<Vec<u8>> {
    let (input, entropy) = (blob(sealed), blob(ENTROPY));
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: as in `protect`; the description is not asked for.
    let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take(out))
}

/// [`protect`] as hex text (for JSON files).
pub fn protect_hex(data: &[u8], description: &str) -> io::Result<String> {
    protect(data, description).map(|sealed| crate::paths::hex(&sealed))
}

/// [`unprotect`] of hex text made by [`protect_hex`].
pub fn unprotect_hex(text: &str) -> io::Result<Vec<u8>> {
    let bytes = crate::paths::unhex(text.trim()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not hex"))?;
    unprotect(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_detects_tampering() {
        let secret = crate::protocol::nonce();
        let sealed = protect(&secret, "test").unwrap();
        assert!(sealed.len() > secret.len());
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_slice()), "the secret is not stored in the clear");
        assert_eq!(unprotect(&sealed).unwrap(), secret);
        let mut broken = sealed.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0x55;
        assert!(unprotect(&broken).is_err(), "a modified blob does not decrypt");
        assert!(unprotect(b"not a blob").is_err());
        let text = protect_hex(b"", "empty").unwrap();
        assert_eq!(unprotect_hex(&text).unwrap(), b"");
        assert!(unprotect_hex("zz").is_err());
    }
}
