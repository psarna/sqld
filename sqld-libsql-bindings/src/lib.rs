#![allow(improper_ctypes)]

pub mod ffi;
pub mod wal_hook;

use std::ops::Deref;

use anyhow::ensure;
#[cfg(feature = "bottomless")]
use once_cell::sync::OnceCell;
use wal_hook::OwnedWalMethods;

use crate::{
    ffi::libsql_wal_methods_register, ffi::libsql_wal_methods_unregister, wal_hook::WalMethodsHook,
};

use self::{
    ffi::{libsql_wal_methods, libsql_wal_methods_find},
    wal_hook::WalHook,
};

pub fn get_orig_wal_methods() -> anyhow::Result<*mut libsql_wal_methods> {
    let orig: *mut libsql_wal_methods = unsafe { libsql_wal_methods_find(std::ptr::null()) };

    if orig.is_null() {
        anyhow::bail!("no underlying methods");
    }

    Ok(orig)
}

pub struct Connection {
    // conn must be dropped first, do not reorder.
    conn: rusqlite::Connection,
    #[allow(dead_code)]
    wal_methods: Option<OwnedWalMethods>,
}

impl Deref for Connection {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

#[cfg(feature = "bottomless")]
static BOTTOMLESS_REPLICATOR: OnceCell<Option<Box<bottomless::replicator::Replicator>>> =
    OnceCell::new();

#[cfg(feature = "bottomless")]
pub async fn init_bottomless_replicator(path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
    tracing::debug!("Initializing bottomless replication");
    let mut replicator =
        bottomless::replicator::Replicator::create(bottomless::replicator::Options {
            create_bucket_if_not_exists: true,
            verify_crc: false,
            use_compression: false,
        })
        .await?;

    // NOTICE: LIBSQL_BOTTOMLESS_DATABASE_ID env variable can be used
    // to pass an additional prefix for the database identifier
    replicator.register_db(
        path.as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid db path"))?
            .to_owned(),
    );

    match replicator.restore().await? {
        bottomless::replicator::RestoreAction::None => (),
        bottomless::replicator::RestoreAction::SnapshotMainDbFile => {
            replicator.new_generation();
            replicator.snapshot_main_db_file().await?;
            // Restoration process only leaves the local WAL file if it was
            // detected to be newer than its remote counterpart.
            replicator.maybe_replicate_wal().await?
        }
        bottomless::replicator::RestoreAction::ReuseGeneration(gen) => {
            replicator.set_generation(gen);
        }
    }

    BOTTOMLESS_REPLICATOR
        .set(Some(Box::new(replicator)))
        .map_err(|_| anyhow::anyhow!("wal_methods initialized twice"))
}

// Registering WAL methods may be subject to race with the later call to libsql_wal_methods_find,
// if we overwrite methods with the same name. A short-term solution is to force register+find
// to be atomic.
// FIXME: a proper solution (Marin is working on it) is to be able to pass user data as a pointer
// directly to libsql_open()
static DB_OPENING_MUTEX: once_cell::sync::Lazy<parking_lot::Mutex<()>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(()));

/// Opens a database with the regular wal methods in the directory pointed to by path
pub fn open_with_regular_wal(
    path: impl AsRef<std::path::Path>,
    flags: rusqlite::OpenFlags,
    wal_hook: impl WalHook + 'static,
    with_bottomless: bool,
) -> anyhow::Result<Connection> {
    let opening_lock = DB_OPENING_MUTEX.lock();
    let path = path.as_ref().join("data");

    assert!(cfg!(feature = "bottomless") || !with_bottomless);

    let mut wal_methods = unsafe {
        let default_methods = get_orig_wal_methods()?;
        #[cfg(feature = "bottomless")]
        let replicator = if with_bottomless {
            // NOTICE: replicator lives as long as the progrma, so it's passed to C code as a pointer
            BOTTOMLESS_REPLICATOR
                .get()
                .ok_or_else(|| anyhow::anyhow!("bottomless replicator not initialized"))?
                .as_ref()
                .map(|r| &*r as *const _ as *mut _)
                .unwrap_or_else(|| std::ptr::null_mut())
        } else {
            std::ptr::null_mut()
        };
        let mut wrapped = WalMethodsHook::wrap(
            default_methods,
            #[cfg(feature = "bottomless")]
            replicator,
            wal_hook,
        );
        let res = libsql_wal_methods_register(wrapped.as_ptr());
        ensure!(res == 0, "failed to register WAL methods");
        wrapped
    };
    tracing::trace!(
        "Opening a connection with regular WAL at {}",
        path.display()
    );
    #[cfg(not(feature = "unix-excl-vfs"))]
    let conn = rusqlite::Connection::open_with_flags_and_wal(
        path,
        flags,
        WalMethodsHook::METHODS_NAME_STR,
    )?;
    #[cfg(feature = "unix-excl-vfs")]
    let conn = rusqlite::Connection::open_with_flags_vfs_and_wal(
        path,
        flags,
        "unix-excl",
        WalMethodsHook::METHODS_NAME_STR,
    )?;
    unsafe {
        libsql_wal_methods_unregister(wal_methods.as_ptr());
    }
    drop(opening_lock);
    conn.pragma_update(None, "journal_mode", "wal")?;

    Ok(Connection {
        conn,
        wal_methods: Some(wal_methods),
    })
}
