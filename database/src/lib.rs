#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate log;

use std::borrow::Cow;
use std::io;
use std::ops::Deref;

use lmdb_zero;

pub use crate::traits::{AsDatabaseBytes, FromDatabaseValue, IntoDatabaseValue};

pub mod lmdb;
pub mod volatile;
pub mod traits;

bitflags! {
    #[derive(Default)]
    pub struct DatabaseFlags: u32 {
        /// Duplicate keys may be used in the database.
        const DUPLICATE_KEYS        = 0b00000001;
        /// This flag may only be used in combination with `DUPLICATE_KEYS`.
        /// This option tells the database that the values for this database are all the same size.
        const DUP_FIXED_SIZE_VALUES = 0b00000010;
        /// Keys are binary integers in native byte order and will be sorted as such
        /// (`std::os::raw::c_uint`, i.e. most likely `u32`).
        const UINT_KEYS             = 0b00000100;
        /// This option specifies that duplicate data items are binary integers, similar to `UINT_KEYS` keys.
        const DUP_UINT_VALUES       = 0b00001000;
    }
}

#[derive(Debug)]
pub enum Environment {
    Volatile(volatile::VolatileEnvironment),
    Persistent(lmdb::LmdbEnvironment),
}

impl Environment {
    pub fn open_database(&self, name: String) -> Database {
        match *self {
            Environment::Volatile(ref env) => { return Database::Volatile(env.open_database(name, Default::default())); }
            Environment::Persistent(ref env) => { return Database::Persistent(env.open_database(name, Default::default())); }
        }
    }

    pub fn open_database_with_flags(&self, name: String, flags: DatabaseFlags) -> Database {
        match *self {
            Environment::Volatile(ref env) => { return Database::Volatile(env.open_database(name, flags)); }
            Environment::Persistent(ref env) => { return Database::Persistent(env.open_database(name, flags)); }
        }
    }

    pub fn close(self) {}

    pub fn drop_database(self) -> io::Result<()> {
        match self {
            Environment::Volatile(_) => { return Ok(()); }
            Environment::Persistent(env) => { return env.drop_database(); }
        }
    }
}

#[derive(Debug)]
pub enum Database<'env> {
    Volatile(volatile::VolatileDatabase<'env>),
    Persistent(lmdb::LmdbDatabase<'env>),
}

impl<'env> Database<'env> {
    fn volatile(&self) -> Option<&volatile::VolatileDatabase> {
        if let Database::Volatile(ref db) = self {
            return Some(db);
        }
        return None;
    }

    fn persistent(&self) -> Option<&lmdb::LmdbDatabase> {
        match self {
            Database::Persistent(ref db) => Some(db),
            Database::Volatile(ref db) => Some(db.as_lmdb()),
        }
    }
}

#[derive(Debug)]
pub enum Transaction<'env> {
    VolatileRead(volatile::VolatileReadTransaction<'env>),
    VolatileWrite(volatile::VolatileWriteTransaction<'env>),
    PersistentRead(lmdb::LmdbReadTransaction<'env>),
    PersistentWrite(lmdb::LmdbWriteTransaction<'env>),
}

impl<'env> Transaction<'env> {
    pub fn get<K, V>(&self, db: &Database, key: &K) -> Option<V> where K: AsDatabaseBytes + ?Sized, V: FromDatabaseValue {
        match *self {
            Transaction::VolatileRead(ref txn) => { return txn.get(db.volatile().unwrap(), key); }
            Transaction::VolatileWrite(ref txn) => { return txn.get(db.volatile().unwrap(), key); }
            Transaction::PersistentRead(ref txn) => { return txn.get(db.persistent().unwrap(), key); }
            Transaction::PersistentWrite(ref txn) => { return txn.get(db.persistent().unwrap(), key); }
        }
    }

    pub fn cursor<'txn, 'db>(&'txn self, db: &'db Database<'env>) -> Cursor<'txn, 'db> {
        match *self {
            Transaction::VolatileRead(ref txn) => { return Cursor::VolatileCursor(txn.cursor(db)); }
            Transaction::VolatileWrite(ref txn) => { return Cursor::VolatileCursor(txn.cursor(db)); }
            Transaction::PersistentRead(ref txn) => { return Cursor::PersistentCursor(txn.cursor(db)); }
            Transaction::PersistentWrite(ref txn) => { return Cursor::PersistentCursor(txn.cursor(db)); }
        }
    }
}

#[derive(Debug)]
pub struct ReadTransaction<'env>(Transaction<'env>);

impl<'env> ReadTransaction<'env> {
    pub fn new(env: &'env Environment) -> Self {
        match *env {
            Environment::Volatile(ref env) => { return ReadTransaction(Transaction::VolatileRead(volatile::VolatileReadTransaction::new(env))); }
            Environment::Persistent(ref env) => { return ReadTransaction(Transaction::PersistentRead(lmdb::LmdbReadTransaction::new(env))); }
        }
    }

    pub fn get<K, V>(&self, db: &Database, key: &K) -> Option<V> where K: AsDatabaseBytes + ?Sized, V: FromDatabaseValue {
        self.0.get(db, key)
    }

    pub fn close(self) {}

    pub fn cursor<'txn, 'db>(&'txn self, db: &'db Database<'env>) -> Cursor<'txn, 'db> {
        self.0.cursor(db)
    }
}

impl<'env> Deref for ReadTransaction<'env> {
    type Target = Transaction<'env>;

    fn deref(&self) -> &Transaction<'env> {
        return &self.0;
    }
}

#[derive(Debug)]
pub struct WriteTransaction<'env>(Transaction<'env>);

impl<'env> WriteTransaction<'env> {
    pub fn new(env: &'env Environment) -> Self {
        match *env {
            Environment::Volatile(ref env) => { return WriteTransaction(Transaction::VolatileWrite(volatile::VolatileWriteTransaction::new(env))); }
            Environment::Persistent(ref env) => { return WriteTransaction(Transaction::PersistentWrite(lmdb::LmdbWriteTransaction::new(env))); }
        }
    }

    pub fn get<K, V>(&self, db: &Database, key: &K) -> Option<V> where K: AsDatabaseBytes + ?Sized, V: FromDatabaseValue {
        self.0.get(db, key)
    }

    /// Puts a key/value pair into the database by copying it into a reserved space in the database.
    /// This works best for values that need to be serialised into the reserved space.
    /// This method will panic when called on a database with duplicate keys!
    pub fn put_reserve<K, V>(&mut self, db: &Database, key: &K, value: &V) where K: AsDatabaseBytes + ?Sized, V: IntoDatabaseValue + ?Sized {
        match self.0 {
            Transaction::VolatileWrite(ref mut txn) => { return txn.put_reserve(db.volatile().unwrap(), key, value); }
            Transaction::PersistentWrite(ref mut txn) => { return txn.put_reserve(db.persistent().unwrap(), key, value); }
            _ => { unreachable!(); }
        }
    }

    /// Puts a key/value pair into the database by passing a reference to a byte slice.
    /// This is more efficient than `put_reserve` if no serialisation is needed,
    /// and the existing value can be immediately written into the database.
    /// This also works with duplicate key databases.
    pub fn put<K, V>(&mut self, db: &Database, key: &K, value: &V) where K: AsDatabaseBytes + ?Sized, V: AsDatabaseBytes + ?Sized {
        match self.0 {
            Transaction::VolatileWrite(ref mut txn) => { return txn.put(db.volatile().unwrap(), key, value); }
            Transaction::PersistentWrite(ref mut txn) => { return txn.put(db.persistent().unwrap(), key, value); }
            _ => { unreachable!(); }
        }
    }

    pub fn remove<K>(&mut self, db: &Database, key: &K) where K: AsDatabaseBytes + ?Sized {
        match self.0 {
            Transaction::VolatileWrite(ref mut txn) => { return txn.remove(db.volatile().unwrap(), key); }
            Transaction::PersistentWrite(ref mut txn) => { return txn.remove(db.persistent().unwrap(), key); }
            _ => { unreachable!(); }
        }
    }

    pub fn remove_item<K, V>(&mut self, db: &Database, key: &K, value: &V) where K: AsDatabaseBytes + ?Sized, V: AsDatabaseBytes + ?Sized {
        match self.0 {
            Transaction::VolatileWrite(ref mut txn) => { return txn.remove_item(db.volatile().unwrap(), key, value); }
            Transaction::PersistentWrite(ref mut txn) => { return txn.remove_item(db.persistent().unwrap(), key, value); }
            _ => { unreachable!(); }
        }
    }

    pub fn commit(self) {
        match self.0 {
            Transaction::VolatileWrite(txn) => { return txn.commit(); }
            Transaction::PersistentWrite(txn) => { return txn.commit(); }
            _ => { unreachable!(); }
        }
    }

    pub fn abort(self) {}

    pub fn cursor<'txn, 'db>(&'txn self, db: &'db Database<'env>) -> Cursor<'txn, 'db> {
        self.0.cursor(db)
    }
}

impl<'env> Deref for WriteTransaction<'env> {
    type Target = Transaction<'env>;

    fn deref(&self) -> &Transaction<'env> {
        return &self.0;
    }
}

pub enum Cursor<'txn, 'db> {
    VolatileCursor(volatile::VolatileCursor<'txn, 'db>),
    PersistentCursor(lmdb::LmdbCursor<'txn, 'db>),
}

macro_rules! gen_cursor_match {
    ($self: ident, $f: ident) => {
        match $self {
            Cursor::PersistentCursor(ref mut cursor) => {
                cursor.$f()
            },
            Cursor::VolatileCursor(ref mut cursor) => {
                cursor.$f()
            },
        }
    };
    ($self: ident, $f: ident, $k: expr) => {
        match $self {
            Cursor::PersistentCursor(ref mut cursor) => {
                cursor.$f($k)
            },
            Cursor::VolatileCursor(ref mut cursor) => {
                cursor.$f($k)
            },
        }
    };
    ($self: ident, $f: ident, $k: expr, $v: expr) => {
        match $self {
            Cursor::PersistentCursor(ref mut cursor) => {
                cursor.$f($k, $v)
            },
            Cursor::VolatileCursor(ref mut cursor) => {
                cursor.$f($k, $v)
            },
        }
    };
}

impl<'txn, 'db> Cursor<'txn, 'db> {
    pub fn first<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, first)
    }

    pub fn first_duplicate<V>(&mut self) -> Option<(V)> where V: FromDatabaseValue {
        gen_cursor_match!(self, first_duplicate)
    }

    pub fn last<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, last)
    }

    pub fn last_duplicate<V>(&mut self) -> Option<(V)> where V: FromDatabaseValue {
        gen_cursor_match!(self, last_duplicate)
    }

    pub fn seek_key_value<K, V>(&mut self, key: &K, value: &V) -> bool where K: AsDatabaseBytes + ?Sized, V: AsDatabaseBytes + ?Sized {
        gen_cursor_match!(self, seek_key_value, key, value)
    }

    pub fn seek_key_nearest_value<K, V>(&mut self, key: &K, value: &V) -> Option<V> where K: AsDatabaseBytes + ?Sized, V: AsDatabaseBytes + FromDatabaseValue {
        gen_cursor_match!(self, seek_key_nearest_value, key, value)
    }

    pub fn get_current<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, get_current)
    }

    pub fn next<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, next)
    }

    pub fn next_duplicate<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, next_duplicate)
    }

    pub fn next_no_duplicate<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, next_no_duplicate)
    }

    pub fn prev<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, prev)
    }

    pub fn prev_duplicate<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, prev_duplicate)
    }

    pub fn prev_no_duplicate<K, V>(&mut self) -> Option<(K, V)> where K: FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, prev_no_duplicate)
    }

    pub fn seek_key<K, V>(&mut self, key: &K) -> Option<V> where K: AsDatabaseBytes + ?Sized, V: FromDatabaseValue {
        gen_cursor_match!(self, seek_key, key)
    }

    pub fn seek_key_both<K, V>(&mut self, key: &K) -> Option<(K, V)> where K: AsDatabaseBytes + FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, seek_key_both, key)
    }

    pub fn seek_range_key<K, V>(&mut self, key: &K) -> Option<(K, V)> where K: AsDatabaseBytes + FromDatabaseValue, V: FromDatabaseValue {
        gen_cursor_match!(self, seek_range_key, key)
    }

    pub fn count_duplicates(&mut self) -> usize {
        gen_cursor_match!(self, count_duplicates)
    }
}
