//! Certificate files (.cer, .crt, .der, .pem): subject, issuer and validity, decoded by Windows
//! (DER or Base64/PEM). Read-only; nothing is imported into a store.

use std::path::Path;

use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CRYPT_STRING_BASE64_ANY, CertCreateCertificateContext, CertFreeCertificateContext, CertGetNameStringW,
    CryptStringToBinaryA, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

#[derive(Debug, Clone, PartialEq)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    /// Unix milliseconds.
    pub not_before: i64,
    pub not_after: i64,
}

const SIMPLE_DISPLAY: u32 = 4; // CERT_NAME_SIMPLE_DISPLAY_TYPE
const ISSUER: u32 = 1; // CERT_NAME_ISSUER_FLAG

fn filetime_ms(low: u32, high: u32) -> i64 {
    let t = ((high as i64) << 32) | low as i64;
    (t - 116_444_736_000_000_000) / 10_000
}

/// DER as is; Base64 / PEM decoded.
fn der(data: &[u8]) -> Option<Vec<u8>> {
    if data.first() == Some(&0x30) {
        return Some(data.to_vec());
    }
    unsafe {
        let mut len = 0u32;
        CryptStringToBinaryA(data, CRYPT_STRING_BASE64_ANY, None, &mut len, None, None).ok()?;
        let mut out = vec![0u8; len as usize];
        CryptStringToBinaryA(data, CRYPT_STRING_BASE64_ANY, Some(out.as_mut_ptr()), &mut len, None, None).ok()?;
        out.truncate(len as usize);
        Some(out)
    }
}

pub fn info(path: &Path) -> Option<CertInfo> {
    let data = std::fs::read(path).ok()?;
    if data.len() > 1 << 20 {
        return None;
    }
    let der = der(&data)?;
    unsafe {
        let ctx = CertCreateCertificateContext(X509_ASN_ENCODING | PKCS_7_ASN_ENCODING, &der);
        if ctx.is_null() {
            return None;
        }
        let name = |flags: u32| -> String {
            let mut buf = [0u16; 512];
            let n = CertGetNameStringW(ctx, SIMPLE_DISPLAY, flags, None, Some(&mut buf));
            String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)])
        };
        let ci = &*(*(ctx as *const CERT_CONTEXT)).pCertInfo;
        let out = CertInfo {
            subject: name(0),
            issuer: name(ISSUER),
            not_before: filetime_ms(ci.NotBefore.dwLowDateTime, ci.NotBefore.dwHighDateTime),
            not_after: filetime_ms(ci.NotAfter.dwLowDateTime, ci.NotAfter.dwHighDateTime),
        };
        let _ = CertFreeCertificateContext(Some(ctx));
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn junk_is_none() {
        let dir = std::env::temp_dir().join(format!("rf-cert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.cer");
        std::fs::write(&f, b"not a certificate").unwrap();
        assert_eq!(super::info(&f), None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(super::filetime_ms(0xd53e8000, 0x019db1de), 0);
    }
}
