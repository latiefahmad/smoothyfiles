//! Authenticode signatures embedded in programs, for the Properties window's Digital signatures
//! tab: who signed, the digest algorithm, the timestamp, nested (dual) signatures, and Windows'
//! own verdict. Read-only, and never touches the network (revocation and downloads are off).

use std::ffi::{CStr, c_void};
use std::path::Path;

use windows::Win32::Foundation::{FILETIME, HLOCAL, HWND, LocalFree};
use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED, CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_CONTENT_TYPE_FLAGS, CERT_QUERY_FORMAT_FLAG_BINARY,
    CERT_QUERY_OBJECT_BLOB, CERT_QUERY_OBJECT_FILE, CMSG_CONTENT_PARAM, CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM, CRYPT_DECODE_ALLOC_FLAG, CRYPT_INTEGER_BLOB,
    CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext, CertGetNameStringW, CryptDecodeObjectEx, CryptMsgClose, CryptMsgGetParam, CryptMsgOpenToDecode,
    CryptMsgUpdate, CryptQueryObject, HCERTSTORE, PKCS7_SIGNER_INFO, CRYPT_TIMESTAMP_INFO, TIMESTAMP_INFO, szOID_RSA_signingTime,
};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE,
    WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
};
use windows::core::{PCSTR, PCWSTR};

use crate::wide::wide;

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    /// Signer's name (certificate subject's simple display name, e.g. "Microsoft Corporation").
    pub signer: String,
    /// Issuer's simple name ("Microsoft Code Signing PCA 2011").
    pub issuer: String,
    /// "sha256", "sha1"… (the signer info's digest algorithm).
    pub digest: String,
    /// Countersignature (timestamp) time in Unix ms, if timestamped (RFC3161 or legacy).
    pub timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signatures {
    /// Embedded signatures: the primary one first, then nested ones (szOID_NESTED_SIGNATURE, e.g. a sha1 + sha256 dual signature).
    pub list: Vec<Signature>,
    /// WinVerifyTrust's verdict for the file: Ok(()) valid, Err(short English reason, e.g. "the signature does not match the file", "the certificate is not trusted", "the certificate has expired").
    pub verdict: Result<(), String>,
}

impl Default for Signatures {
    fn default() -> Self {
        Signatures { list: Vec::new(), verdict: Ok(()) }
    }
}

impl Default for Signature {
    fn default() -> Self {
        Signature { signer: String::new(), issuer: String::new(), digest: String::new(), timestamp_ms: None }
    }
}

const ENCODING: u32 = 0x0001_0001; // X509_ASN_ENCODING | PKCS_7_ASN_ENCODING
const OID_NESTED_SIGNATURE: &str = "1.3.6.1.4.1.311.2.4.1";
const OID_RFC3161_TIMESTAMP: &str = "1.3.6.1.4.1.311.3.3.1";
const OID_COUNTER_SIGN: &str = "1.2.840.113549.1.9.6";

/// A FILETIME as Unix milliseconds.
fn filetime_ms(ft: FILETIME) -> i64 {
    let t = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
    (t - 116_444_736_000_000_000) / 10_000
}

fn digest_name(oid: &str) -> String {
    match oid {
        "2.16.840.1.101.3.4.2.1" | "2.16.840.1.101.3.4.2.1.0" => "sha256".into(),
        "1.3.14.3.2.26" => "sha1".into(),
        "2.16.840.1.101.3.4.2.2" => "sha384".into(),
        "2.16.840.1.101.3.4.2.3" => "sha512".into(),
        "1.2.840.113549.2.5" => "md5".into(),
        other => other.to_string(),
    }
}

/// Short English reason for a WinVerifyTrust result.
fn verdict_text(hr: i32) -> Result<(), String> {
    let msg = match hr as u32 {
        0 => return Ok(()),
        0x8009_6010 => "the signature does not match the file",
        0x800B_0109 | 0x800B_0112 => "the certificate is not trusted",
        0x800B_010D => "the certificate chains to a test root that is not trusted",
        0x800B_0101 => "the certificate has expired",
        0x800B_0100 => "the file has no signature",
        0x800B_010A => "the certificate chain could not be built",
        0x800B_0111 => "the signer is explicitly distrusted",
        0x800B_010C => "the certificate has been revoked",
        0x800B_0106 => "the certificate is not valid for code signing",
        0x800B_0003 => "the file type is not supported",
        0x8009_6004 => "the certificate's signature is not valid",
        other => return Err(format!("error 0x{other:08X}")),
    };
    Err(msg.to_string())
}

unsafe fn oid_text(p: *const u8) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p.cast()) }.to_string_lossy().into_owned() }
}

unsafe fn blob<'a>(b: &CRYPT_INTEGER_BLOB) -> &'a [u8] {
    if b.pbData.is_null() || b.cbData == 0 { &[] } else { unsafe { std::slice::from_raw_parts(b.pbData, b.cbData as usize) } }
}

/// CryptDecodeObjectEx with Windows allocating the result; free the pointer with LocalFree.
unsafe fn decode_alloc(struct_type: PCSTR, data: &[u8]) -> Option<*mut c_void> {
    let mut out: *mut c_void = std::ptr::null_mut();
    let mut size = 0u32;
    unsafe {
        CryptDecodeObjectEx(
            windows::Win32::Security::Cryptography::CERT_QUERY_ENCODING_TYPE(ENCODING),
            struct_type,
            data,
            CRYPT_DECODE_ALLOC_FLAG,
            None,
            Some((&mut out as *mut *mut c_void).cast()),
            &mut size,
        )
        .ok()?;
    }
    (!out.is_null()).then_some(out)
}

unsafe fn free_local(p: *mut c_void) {
    let _ = unsafe { LocalFree(Some(HLOCAL(p))) };
}

/// Time of a legacy countersignature: the signing-time attribute of the nested signer info.
unsafe fn legacy_time(encoded: &[u8]) -> Option<i64> {
    unsafe {
        let p = decode_alloc(PKCS7_SIGNER_INFO, encoded)?;
        let info = &*(p as *const CMSG_SIGNER_INFO);
        let mut found = None;
        for i in 0..info.AuthAttrs.cAttr as usize {
            let attr = &*info.AuthAttrs.rgAttr.add(i);
            if oid_text(attr.pszObjId.0) != "1.2.840.113549.1.9.5" || attr.cValue == 0 {
                continue;
            }
            let v = &*attr.rgValue;
            let mut ft = FILETIME::default();
            let mut size = std::mem::size_of::<FILETIME>() as u32;
            let ok = CryptDecodeObjectEx(
                windows::Win32::Security::Cryptography::CERT_QUERY_ENCODING_TYPE(ENCODING),
                szOID_RSA_signingTime,
                blob(v),
                0,
                None,
                Some((&mut ft as *mut FILETIME).cast()),
                &mut size,
            )
            .is_ok();
            if ok {
                found = Some(filetime_ms(ft));
                break;
            }
        }
        free_local(p);
        found
    }
}

/// Time of an RFC 3161 timestamp token: a PKCS#7 message whose content is the TSTInfo.
unsafe fn rfc3161_time(encoded: &[u8]) -> Option<i64> {
    unsafe {
        let msg = CryptMsgOpenToDecode(ENCODING, 0, 0, None, None, None);
        if msg.is_null() {
            return None;
        }
        let mut result = None;
        if CryptMsgUpdate(msg, Some(encoded), true).is_ok() {
            let mut len = 0u32;
            if CryptMsgGetParam(msg, CMSG_CONTENT_PARAM, 0, None, &mut len).is_ok() && len > 0 {
                let mut content = vec![0u8; len as usize];
                if CryptMsgGetParam(msg, CMSG_CONTENT_PARAM, 0, Some(content.as_mut_ptr().cast()), &mut len).is_ok() {
                    content.truncate(len as usize);
                    if let Some(p) = decode_alloc(TIMESTAMP_INFO, &content) {
                        result = Some(filetime_ms((*(p as *const CRYPT_TIMESTAMP_INFO)).ftTime));
                        free_local(p);
                    }
                }
            }
        }
        let _ = CryptMsgClose(Some(msg));
        result
    }
}

/// The first signer of a message: names from its certificate, digest and timestamp, plus the raw
/// nested signatures (copied out) it carries.
unsafe fn parse_signer(msg: *const c_void, store: HCERTSTORE) -> Option<(Signature, Vec<Vec<u8>>)> {
    unsafe {
        let mut len = 0u32;
        CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, None, &mut len).ok()?;
        // u64 words so the CMSG_SIGNER_INFO inside is properly aligned
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, Some(buf.as_mut_ptr().cast()), &mut len).ok()?;
        let info = &*(buf.as_ptr() as *const CMSG_SIGNER_INFO);

        let mut sig = Signature { digest: digest_name(&oid_text(info.HashAlgorithm.pszObjId.0)), ..Default::default() };

        let key = CERT_INFO { Issuer: info.Issuer, SerialNumber: info.SerialNumber, ..Default::default() };
        let ctx = CertFindCertificateInStore(
            store,
            windows::Win32::Security::Cryptography::CERT_QUERY_ENCODING_TYPE(ENCODING),
            0,
            CERT_FIND_SUBJECT_CERT,
            Some((&key as *const CERT_INFO).cast()),
            None,
        );
        if !ctx.is_null() {
            let name = |flags: u32| -> String {
                let mut b = [0u16; 512];
                let n = CertGetNameStringW(ctx as *const CERT_CONTEXT, 4, flags, None, Some(&mut b));
                String::from_utf16_lossy(&b[..(n as usize).saturating_sub(1)])
            };
            sig.signer = name(0);
            sig.issuer = name(1);
            let _ = CertFreeCertificateContext(Some(ctx));
        }

        let mut nested = Vec::new();
        for i in 0..info.UnauthAttrs.cAttr as usize {
            let attr = &*info.UnauthAttrs.rgAttr.add(i);
            let oid = oid_text(attr.pszObjId.0);
            for j in 0..attr.cValue as usize {
                let v = blob(&*attr.rgValue.add(j));
                match oid.as_str() {
                    OID_NESTED_SIGNATURE => nested.push(v.to_vec()),
                    OID_RFC3161_TIMESTAMP if sig.timestamp_ms.is_none() => sig.timestamp_ms = rfc3161_time(v),
                    OID_COUNTER_SIGN if sig.timestamp_ms.is_none() => sig.timestamp_ms = legacy_time(v),
                    _ => {}
                }
            }
        }
        Some((sig, nested))
    }
}

/// Opens a signed message (embedded in a file, or a bare PKCS#7 blob), hands it to `f`, closes it.
unsafe fn with_message<T>(file: Option<&[u16]>, bytes: Option<&[u8]>, f: impl FnOnce(*const c_void, HCERTSTORE) -> Option<T>) -> Option<T> {
    let mut store = HCERTSTORE::default();
    let mut msg: *mut c_void = std::ptr::null_mut();
    let b = bytes.map(|b| CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 });
    unsafe {
        let ok = match (file, b.as_ref()) {
            (Some(path), _) => CryptQueryObject(
                CERT_QUERY_OBJECT_FILE,
                path.as_ptr().cast(),
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                None,
                None,
                None,
                Some(&mut store),
                Some(&mut msg),
                None,
            ),
            (None, Some(blob)) => CryptQueryObject(
                CERT_QUERY_OBJECT_BLOB,
                (blob as *const CRYPT_INTEGER_BLOB).cast(),
                CERT_QUERY_CONTENT_TYPE_FLAGS(CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED.0 | CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED.0),
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                None,
                None,
                None,
                Some(&mut store),
                Some(&mut msg),
                None,
            ),
            _ => return None,
        };
        if ok.is_err() {
            return None;
        }
        let r = if msg.is_null() { None } else { f(msg, store) };
        if !msg.is_null() {
            let _ = CryptMsgClose(Some(msg));
        }
        if !store.0.is_null() {
            let _ = CertCloseStore(Some(store), 0);
        }
        r
    }
}

/// WinVerifyTrust on the file, offline: cached revocation data only, no UI.
fn verify(path: &[u16]) -> Result<(), String> {
    let mut file = WINTRUST_FILE_INFO { cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32, pcwszFilePath: PCWSTR(path.as_ptr()), ..Default::default() };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let hr = unsafe {
        // -1 as the window means "no UI at all"
        let no_ui = HWND(-1isize as *mut c_void);
        let hr = WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast());
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast());
        hr
    };
    verdict_text(hr)
}

/// None when the file carries no embedded signature (catalog-signed system files included:
/// Explorer shows no tab for those either).
pub fn read(path: &Path) -> Option<Signatures> {
    let w = wide(path);
    let (first, nested) = unsafe { with_message(Some(&w), None, |msg, store| parse_signer(msg, store)) }?;
    let mut list = vec![first];
    for n in nested {
        if let Some((sig, _)) = unsafe { with_message(None, Some(&n), |msg, store| parse_signer(msg, store)) } {
            list.push(sig);
        }
    }
    Some(Signatures { list, verdict: verify(&w) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_file_is_none() {
        let dir = std::env::temp_dir().join(format!("rf-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("plain.exe");
        std::fs::write(&f, b"MZ not really a program").unwrap();
        let r = read(&f);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r, None);
    }

    #[test]
    fn verdicts_have_short_reasons() {
        assert_eq!(verdict_text(0), Ok(()));
        assert_eq!(verdict_text(0x80096010u32 as i32), Err("the signature does not match the file".into()));
        assert_eq!(verdict_text(0x800B0109u32 as i32), Err("the certificate is not trusted".into()));
        assert_eq!(verdict_text(0x800B0101u32 as i32), Err("the certificate has expired".into()));
        assert_eq!(verdict_text(0x12345678), Err("error 0x12345678".into()));
        assert_eq!(digest_name("2.16.840.1.101.3.4.2.1"), "sha256");
        assert_eq!(digest_name("1.3.14.3.2.26"), "sha1");
        assert_eq!(digest_name("9.9"), "9.9");
    }

    #[test]
    fn reads_an_embedded_signature() {
        let candidates = [
            r"C:\Windows\System32\ntoskrnl.exe",
            r"C:\Program Files\Windows Defender\MsMpEng.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r"C:\Windows\explorer.exe",
            r"C:\Windows\System32\notepad.exe",
        ];
        let mut checked = false;
        for c in candidates {
            let p = Path::new(c);
            if !p.exists() {
                continue;
            }
            let Some(s) = read(p) else { continue };
            checked = true;
            let first = &s.list[0];
            assert!(!first.signer.is_empty(), "{c}: {s:?}");
            assert!(!first.issuer.is_empty(), "{c}: {s:?}");
            assert!(first.digest == "sha256" || first.digest == "sha1", "{c}: {s:?}");
            assert_eq!(s.verdict, Ok(()), "{c}: {s:?}");
            assert!(s.list.iter().any(|x| x.timestamp_ms.is_some()), "{c}: {s:?}");
            eprintln!("{c}: {s:?}");
            break;
        }
        if !checked {
            eprintln!("no embedded-signed candidate on this machine, skipped");
        }
    }
}
