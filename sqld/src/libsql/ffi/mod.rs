#![allow(dead_code)]

pub mod types;

use std::ffi::{c_char, c_int, c_void};

use types::*;

pub const SQLITE_OK: i32 = 0;
pub const SQLITE_CANTOPEN: i32 = 14;
pub const SQLITE_IOERR_WRITE: i32 = 778;

pub const SQLITE_CHECKPOINT_FULL: i32 = 1;

pub use rusqlite::ffi::{
    libsql_wal_methods, libsql_wal_methods_find, libsql_wal_methods_register, sqlite3_file,
    sqlite3_io_methods, sqlite3_vfs, PgHdr, Wal, WalIndexHdr, SQLITE_CANTOPEN,
    SQLITE_CHECKPOINT_FULL, SQLITE_IOERR_WRITE, SQLITE_OK,
};

pub struct PageHdrIter {
    current_ptr: *const PgHdr,
    page_size: usize,
}

impl PageHdrIter {
    pub fn new(current_ptr: *const PgHdr, page_size: usize) -> Self {
        Self {
            current_ptr,
            page_size,
        }
    }
}

impl std::iter::Iterator for PageHdrIter {
    type Item = (u32, &'static [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_ptr.is_null() {
            return None;
        }
        let current_hdr: &PgHdr = unsafe { &*self.current_ptr };
        let raw_data =
            unsafe { std::slice::from_raw_parts(current_hdr.data as *const u8, self.page_size) };
        let item = Some((current_hdr.pgno, raw_data));
        self.current_ptr = current_hdr.dirty;
        item
    }
}
