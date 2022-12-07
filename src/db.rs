use std::{mem::MaybeUninit, os::unix::prelude::OsStrExt, path::Path, pin::Pin, sync::Arc};

use autorocks_sys::{
    new_transaction_db_options, new_write_batch,
    rocksdb::{
        CompressionType, PinnableSlice, ReadOptions, TransactionDBOptions, TransactionOptions,
        WriteOptions,
    },
    DbOptionsWrapper, ReadOnlyDbWrapper, TransactionDBWrapper, TransactionWrapper,
};
use moveit::{moveit, Emplace, New};

use crate::{
    into_result, slice::as_rust_slice, DbIterator, Direction, Result, Snapshot, Transaction,
    WriteBatch,
};

pub struct DbOptions {
    inner: Pin<Box<DbOptionsWrapper>>,
}

impl DbOptions {
    pub fn new(path: &Path, columns: usize) -> Self {
        Self {
            inner: Box::emplace(DbOptionsWrapper::new2(
                path.as_os_str().as_bytes().into(),
                columns,
            )),
        }
    }

    /// Note that this resets all options and column families.
    pub fn load_options_from_file(&mut self, options_file: &Path) -> Result<()> {
        moveit! {
            let status = self.inner.as_mut().load(options_file.as_os_str().as_bytes().into());
        }
        into_result(&status)
    }

    pub fn create_if_missing(&mut self, val: bool) -> &mut Self {
        self.inner.as_mut().set_create_if_missing(val);
        self
    }

    pub fn create_missing_column_families(&mut self, val: bool) -> &mut Self {
        self.inner.as_mut().set_create_missing_column_families(val);
        self
    }

    /// The corresponding feature must be enabled for this to actually work.
    pub fn compression(&mut self, c: CompressionType) -> &mut Self {
        self.inner.as_mut().set_compression(c);
        self
    }

    pub fn repair(&self) -> Result<()> {
        moveit! {
            let status = self.inner.repair();
        }
        into_result(&status)
    }

    pub fn open_read_only(&self) -> Result<ReadOnlyDb> {
        ReadOnlyDb::open(&self.inner)
    }

    pub fn open(&self) -> Result<TransactionDb> {
        moveit! {
            let txn_db_options = new_transaction_db_options();
        }
        TransactionDb::open(&self.inner, &txn_db_options)
    }
}

#[derive(Clone)]
pub struct TransactionDb {
    inner: Arc<TransactionDBWrapper>,
}

impl TransactionDb {
    fn open(
        options: &DbOptionsWrapper,
        txn_db_options: &TransactionDBOptions,
    ) -> Result<TransactionDb> {
        let db = Arc::emplace(TransactionDBWrapper::new());
        let mut db = Pin::into_inner(db);
        let db_mut = Arc::get_mut(&mut db).unwrap();
        moveit! {
            let status = Pin::new(db_mut).open(options, txn_db_options);
        }
        into_result(&status)?;
        Ok(TransactionDb { inner: db })
    }

    pub fn put(&self, col: usize, key: &[u8], value: &[u8]) -> Result<()> {
        moveit! {
            let options = WriteOptions::new();
        }
        self.put_with_options(&options, col, key, value)
    }

    pub fn put_with_options(
        &self,
        options: &WriteOptions,
        col: usize,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        moveit! {
            let status = unsafe { self.inner.put(options, cf, &key.into(), &value.into()) };
        }
        into_result(&status)
    }

    pub fn delete_with_options(
        &self,
        options: &WriteOptions,
        col: usize,
        key: &[u8],
    ) -> Result<()> {
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        moveit! {
            let status = unsafe { self.inner.del(options, cf, &key.into()) };
        }
        into_result(&status)
    }

    pub fn delete(&self, col: usize, key: &[u8]) -> Result<()> {
        moveit! {
            let options = WriteOptions::new();
        }
        self.delete_with_options(&options, col, key)
    }

    pub fn get<'b>(
        &self,
        col: usize,
        key: &[u8],
        buf: Pin<&'b mut PinnableSlice>,
    ) -> Result<Option<&'b [u8]>> {
        moveit! {
            let options = ReadOptions::new();
        }
        self.get_with_options(&options, col, key, buf)
    }

    pub fn get_with_options<'b>(
        &self,
        options: &ReadOptions,
        col: usize,
        key: &[u8],
        buf: Pin<&'b mut PinnableSlice>,
    ) -> Result<Option<&'b [u8]>> {
        let slice = unsafe { buf.get_unchecked_mut() };
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        moveit! {
            let status = unsafe { self.inner.get(options, cf, &key.into(), slice) };
        }
        if status.IsNotFound() {
            return Ok(None);
        }
        into_result(&status)?;
        Ok(Some(as_rust_slice(slice)))
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            inner: self.inner.get_snapshot(),
            db: self.clone(),
        }
    }

    /// Begin transaction with default options (but set_snapshot = true).
    pub fn begin_transaction(&self) -> Transaction {
        moveit! {
            let write_options = WriteOptions::new();
            let mut transaction_options = TransactionOptions::new();
        }
        transaction_options.set_snapshot = true;
        self.begin_transaction_with_options(&write_options, &transaction_options)
    }

    pub fn begin_transaction_with_options(
        &self,
        write_options: &WriteOptions,
        transaction_options: &TransactionOptions,
    ) -> Transaction {
        let mut tx: MaybeUninit<TransactionWrapper> = MaybeUninit::uninit();
        unsafe {
            self.inner
                .begin(write_options, transaction_options)
                .new(Pin::new(&mut tx))
        };
        Transaction {
            inner: unsafe { tx.assume_init() },
            db: self.clone(),
        }
    }

    pub fn iter(&self, col: usize, dir: Direction) -> DbIterator<&'_ Self> {
        moveit! {
            let options = ReadOptions::new();
        }
        self.iter_with_options(&options, col, dir)
    }

    pub fn iter_with_options<'a>(
        &'a self,
        options: &ReadOptions,
        col: usize,
        dir: Direction,
    ) -> DbIterator<&'a Self> {
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        let iter = unsafe { self.as_inner().iter(options, cf) };
        DbIterator::new(iter, dir)
    }

    pub fn new_write_batch(&self) -> WriteBatch {
        WriteBatch {
            inner: new_write_batch(),
            db: self.clone(),
        }
    }

    pub fn write_with_options(
        &self,
        options: &WriteOptions,
        updates: &mut WriteBatch,
    ) -> Result<()> {
        moveit! {
            let status = unsafe {
                self.inner.write(options, updates.as_inner_mut().get_unchecked_mut())
            };
        }
        into_result(&status)
    }

    pub fn write(&self, updates: &mut WriteBatch) -> Result<()> {
        moveit! {
            let options = WriteOptions::new();
        }
        self.write_with_options(&options, updates)
    }

    pub fn as_inner(&self) -> &TransactionDBWrapper {
        &self.inner
    }
}

#[derive(Clone)]
pub struct ReadOnlyDb {
    inner: Arc<ReadOnlyDbWrapper>,
}

impl ReadOnlyDb {
    fn open(options: &DbOptionsWrapper) -> Result<ReadOnlyDb> {
        let db = Arc::emplace(ReadOnlyDbWrapper::new());
        let mut db = Pin::into_inner(db);
        let db_mut = Arc::get_mut(&mut db).unwrap();
        moveit! {
            let status = Pin::new(db_mut).open(options);
        }
        into_result(&status)?;
        Ok(ReadOnlyDb { inner: db })
    }

    pub fn get<'b>(
        &self,
        col: usize,
        key: &[u8],
        buf: Pin<&'b mut PinnableSlice>,
    ) -> Result<Option<&'b [u8]>> {
        moveit! {
            let options = ReadOptions::new();
        }
        self.get_with_options(&options, col, key, buf)
    }

    pub fn get_with_options<'b>(
        &self,
        options: &ReadOptions,
        col: usize,
        key: &[u8],
        buf: Pin<&'b mut PinnableSlice>,
    ) -> Result<Option<&'b [u8]>> {
        let slice = unsafe { buf.get_unchecked_mut() };
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        moveit! {
            let status = unsafe { self.inner.get(options, cf, &key.into(), slice) };
        }
        if status.IsNotFound() {
            return Ok(None);
        }
        into_result(&status)?;
        Ok(Some(as_rust_slice(slice)))
    }

    pub fn iter(&self, col: usize, dir: Direction) -> DbIterator<&'_ Self> {
        moveit! {
            let options = ReadOptions::new();
        }
        self.iter_with_options(&options, col, dir)
    }

    pub fn iter_with_options<'a>(
        &'a self,
        options: &ReadOptions,
        col: usize,
        dir: Direction,
    ) -> DbIterator<&'a Self> {
        let cf = self.inner.get_cf(col);
        assert!(!cf.is_null());
        let iter = unsafe { self.as_inner().iter(options, cf) };
        DbIterator::new(iter, dir)
    }

    pub fn as_inner(&self) -> &ReadOnlyDbWrapper {
        &self.inner
    }
}
