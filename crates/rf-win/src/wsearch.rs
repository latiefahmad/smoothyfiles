//! Windows Search (the system index) — what Files and Explorer use for fast name *and content*
//! search with AQS ("kind:picture", "size:>10MB", "datemodified:this week").
//!
//! The query helper of the index turns the user's AQS into SQL (Windows handles the syntax,
//! localized keywords included) and the SQL runs through the index's OLE DB provider. Only used
//! for folders inside the crawl scope: for anything else Windows would fall back to a slow
//! crawl, and our own walker is faster. Blocking: call off the UI thread.

use std::path::{Path, PathBuf};

use rf_core::{Entry, EntryKind};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Search::{
    CSearchManager, DBACCESSOR_ROWDATA, DBBINDING, DBMEMOWNER_CLIENTOWNED, DBPARAMIO_NOTPARAM, DBPART_LENGTH, DBPART_STATUS,
    DBPART_VALUE, DBSTATUS_S_OK, DBTYPE_FILETIME, DBTYPE_UI4, DBTYPE_UI8, DBTYPE_WSTR, HACCESSOR, IAccessor, ICommandText,
    IDBCreateCommand, IDBCreateSession, IDBInitialize, IRowset, ISearchManager, ISearchQueryHelper,
};
use windows::core::{GUID, Interface, PCWSTR, w};

use crate::search::SearchHit;
use crate::shell::ensure_com;
use crate::wide::wide;

const CLSID_COLLATOR_DSO: GUID = GUID::from_u128(0x9e175b8b_f52a_11d8_b9a5_505054503030);
const DBGUID_DEFAULT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

fn manager() -> windows::core::Result<ISearchManager> {
    unsafe { CoCreateInstance(&CSearchManager, None, CLSCTX_LOCAL_SERVER) }
}

/// Whether `root` is inside the Windows Search crawl scope (and the service answers).
pub fn is_indexed(root: &Path) -> bool {
    ensure_com();
    let r = (|| -> windows::core::Result<bool> {
        unsafe {
            let cat = manager()?.GetCatalog(w!("SystemIndex"))?;
            let scope = cat.GetCrawlScopeManager()?;
            let mut s = root.to_string_lossy().to_string();
            if !s.ends_with('\\') {
                s.push('\\');
            }
            let url = wide(format!("file:///{s}"));
            Ok(scope.IncludedInCrawlScope(PCWSTR(url.as_ptr()))?.as_bool())
        }
    })();
    r.unwrap_or(false)
}

/// The SQL for `query` restricted to `root` (and below).
fn sql_for(root: &Path, query: &str, max: usize) -> anyhow::Result<String> {
    unsafe {
        let helper: ISearchQueryHelper = manager()?.GetCatalog(w!("SystemIndex"))?.GetQueryHelper()?;
        helper.SetQuerySelectColumns(w!("System.ItemPathDisplay,System.Size,System.DateModified,System.DateCreated,System.FileAttributes"))?;
        let scope = root.to_string_lossy().replace('\'', "''");
        let restrict = wide(format!("AND SCOPE='file:{scope}'"));
        helper.SetQueryWhereRestrictions(PCWSTR(restrict.as_ptr()))?;
        helper.SetQueryMaxResults(max.min(i32::MAX as usize) as i32)?;
        let q = wide(query);
        let sql = helper.GenerateSQLFromUserQuery(PCWSTR(q.as_ptr()))?;
        let s = sql.to_string()?;
        CoTaskMemFree(Some(sql.0 as *const _));
        Ok(s)
    }
}

// Row buffer layout (one binding per column: value, length, status).
const PATH_CHARS: usize = 1024;
const OFF_PATH: usize = 0;
const OFF_PATH_LEN: usize = PATH_CHARS * 2;
const OFF_PATH_ST: usize = OFF_PATH_LEN + 8;
const OFF_SIZE: usize = OFF_PATH_ST + 8;
const OFF_SIZE_ST: usize = OFF_SIZE + 8;
const OFF_MOD: usize = OFF_SIZE_ST + 8;
const OFF_MOD_ST: usize = OFF_MOD + 8;
const OFF_CRE: usize = OFF_MOD_ST + 8;
const OFF_CRE_ST: usize = OFF_CRE + 8;
const OFF_ATTR: usize = OFF_CRE_ST + 8;
const OFF_ATTR_ST: usize = OFF_ATTR + 4;
const ROW: usize = OFF_ATTR_ST + 4;

fn binding(ordinal: usize, ty: u16, value: usize, len: Option<usize>, status: usize, max: usize) -> DBBINDING {
    DBBINDING {
        iOrdinal: ordinal,
        obValue: value,
        obLength: len.unwrap_or(0),
        obStatus: status,
        dwPart: (DBPART_VALUE.0 | DBPART_STATUS.0 | if len.is_some() { DBPART_LENGTH.0 } else { 0 }) as u32,
        dwMemOwner: DBMEMOWNER_CLIENTOWNED.0 as u32,
        eParamIO: DBPARAMIO_NOTPARAM.0 as u32,
        cbMaxLen: max,
        wType: ty,
        ..Default::default()
    }
}

fn rd<T: Copy>(buf: &[u8], off: usize) -> T {
    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const T) }
}

fn parse_row(buf: &[u8]) -> Option<SearchHit> {
    if rd::<u32>(buf, OFF_PATH_ST) != DBSTATUS_S_OK.0 as u32 {
        return None;
    }
    let bytes = rd::<usize>(buf, OFF_PATH_LEN).min(PATH_CHARS * 2 - 2);
    let units: Vec<u16> = (0..bytes / 2).map(|i| rd::<u16>(buf, OFF_PATH + i * 2)).collect();
    let path = PathBuf::from(String::from_utf16_lossy(&units));
    let ok = |st: usize| rd::<u32>(buf, st) == DBSTATUS_S_OK.0 as u32;
    let ft = |off: usize, st: usize| if ok(st) { rd::<u64>(buf, off) } else { 0 };
    let attrs = if ok(OFF_ATTR_ST) { rd::<u32>(buf, OFF_ATTR) } else { 0x20 };
    let dir = attrs & 0x10 != 0;
    let size = if ok(OFF_SIZE_ST) && !dir { rd::<u64>(buf, OFF_SIZE) } else { 0 };
    let ms = |v: u64| if v == 0 { 0 } else { rf_core::format::filetime_to_unix_ms(v) };
    let name = path.file_name()?.to_string_lossy().into_owned();
    let kind = if dir { EntryKind::Dir } else { EntryKind::File };
    Some(SearchHit { entry: Entry::new(&name, kind, size, ms(ft(OFF_MOD, OFF_MOD_ST)), ms(ft(OFF_CRE, OFF_CRE_ST)), attrs), path })
}

/// Runs `query` (AQS or plain words; names and contents) over `root` and its subfolders
/// through the index. Returns the number of hits.
pub fn search(
    root: &Path,
    query: &str,
    max_results: usize,
    cancelled: &dyn Fn() -> bool,
    on_batch: &mut dyn FnMut(Vec<SearchHit>),
) -> anyhow::Result<usize> {
    ensure_com();
    let sql = sql_for(root, query, max_results)?;
    let mut n = 0usize;
    unsafe {
        let init: IDBInitialize = CoCreateInstance(&CLSID_COLLATOR_DSO, None, CLSCTX_INPROC_SERVER)?;
        init.Initialize()?;
        let session: IDBCreateCommand = init.cast::<IDBCreateSession>()?.CreateSession(None, &IDBCreateCommand::IID)?.cast()?;
        let cmd: ICommandText = session.CreateCommand(None, &ICommandText::IID)?.cast()?;
        let wsql = wide(&sql);
        cmd.SetCommandText(&DBGUID_DEFAULT, PCWSTR(wsql.as_ptr()))?;
        let mut unk: Option<windows::core::IUnknown> = None;
        cmd.Execute(None, &IRowset::IID, None, None, Some(&mut unk))?;
        let rowset: IRowset = unk.ok_or_else(|| anyhow::anyhow!("no rowset"))?.cast()?;
        let accessor: IAccessor = rowset.cast()?;
        let bindings = [
            binding(1, DBTYPE_WSTR.0 as u16, OFF_PATH, Some(OFF_PATH_LEN), OFF_PATH_ST, PATH_CHARS * 2),
            binding(2, DBTYPE_UI8.0 as u16, OFF_SIZE, None, OFF_SIZE_ST, 8),
            binding(3, DBTYPE_FILETIME.0 as u16, OFF_MOD, None, OFF_MOD_ST, 8),
            binding(4, DBTYPE_FILETIME.0 as u16, OFF_CRE, None, OFF_CRE_ST, 8),
            binding(5, DBTYPE_UI4.0 as u16, OFF_ATTR, None, OFF_ATTR_ST, 4),
        ];
        let mut hacc = HACCESSOR::default();
        accessor.CreateAccessor(DBACCESSOR_ROWDATA.0 as u32, bindings.len(), bindings.as_ptr(), ROW, &mut hacc, None)?;
        let mut buf = vec![0u8; ROW];
        const CHUNK: usize = 64;
        let mut hrows = [0usize; CHUNK];
        let mut batch = Vec::new();
        'rows: loop {
            if cancelled() {
                break;
            }
            // the provider fills the array *prghRows points to (the first slot of this slice)
            let mut arg: [*mut usize; CHUNK] = [std::ptr::null_mut(); CHUNK];
            arg[0] = hrows.as_mut_ptr();
            let mut got = 0usize;
            let r = rowset.GetNextRows(0, 0, &mut got, &mut arg);
            if got == 0 {
                break;
            }
            for &h in hrows.iter().take(got) {
                buf.fill(0);
                if rowset.GetData(h, hacc, buf.as_mut_ptr() as *mut _).is_ok() {
                    if let Some(hit) = parse_row(&buf) {
                        batch.push(hit);
                        n += 1;
                    }
                }
            }
            let _ = rowset.ReleaseRows(got, hrows.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut());
            if batch.len() >= 128 {
                on_batch(std::mem::take(&mut batch));
            }
            if r.is_err() || n >= max_results {
                break 'rows;
            }
        }
        if !batch.is_empty() {
            on_batch(batch);
        }
        let _ = accessor.ReleaseAccessor(hacc, None);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual: `cargo test -p rf-win wsearch_live -- --ignored --nocapture` (RF_Q=query)
    #[test]
    #[ignore]
    fn wsearch_live() {
        let root = PathBuf::from(std::env::var("USERPROFILE").unwrap());
        println!("indexed={}", is_indexed(&root));
        let q = std::env::var("RF_Q").unwrap_or_else(|_| "kind:picture".into());
        println!("sql={}", sql_for(&root, &q, 20).unwrap());
        let mut all = Vec::new();
        let n = search(&root, &q, 20, &|| false, &mut |b| all.extend(b)).unwrap();
        println!("hits={n}");
        for h in all.iter().take(5) {
            println!("{} {} {} {:x}", h.path.display(), h.entry.size, h.entry.modified_ms, h.entry.attrs);
        }
    }
}
