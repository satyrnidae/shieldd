//! Windows DPAPI: encryption tied to the current user's login, so only they can undo it on this
//! machine. No key to keep anywhere.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

/// Mixed into every blob, so another app's DPAPI data can't pass for ours or the reverse
const ENTROPY: &[u8] = b"shieldd p2p.conf psk";

fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr().cast_mut() }
}

/// Copies DPAPI's output out and frees it
fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: DPAPI filled out w/ a LocalAlloc'd buffer of cbData bytes, freed right after
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        v
    }
}

pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: the input blobs borrow slices that outlive the call; out is DPAPI's to fill
    unsafe {
        CryptProtectData(
            &blob(plain),
            w!("shieldd group key"),
            Some(&blob(ENTROPY)),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    }
    .context("encrypting w/ DPAPI")?;
    Ok(take(out))
}

pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as protect
    unsafe {
        CryptUnprotectData(&blob(sealed), None, Some(&blob(ENTROPY)), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    }
    .context("decrypting w/ DPAPI; it only opens for the Windows user who saved it, on that machine")?;
    Ok(take(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_tampering() {
        let sealed = protect(b"s3cretPW").unwrap();
        assert!(!sealed.windows(8).any(|w| w == b"s3cretPW"));
        assert_eq!(unprotect(&sealed).unwrap(), b"s3cretPW");
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(unprotect(&bad).is_err());
    }
}
