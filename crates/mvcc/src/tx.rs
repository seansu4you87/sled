use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};
use std::sync::Arc;

use sled::Error as DbError;

use super::*;

pub(super) type PredicateFn = fn(&Key, &Option<Value>) -> bool;
pub(super) struct Predicate(Key, Box<PredicateFn>);
pub(super) struct Write(Key, Option<Value>);

pub type TxResult = Result<(), Error>;

#[derive(Debug, PartialEq)]
pub enum Error {
    Abort,
    PredicateFailure,
    Db(DbError<()>),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::Abort => write!(f, "Transaction aborted"),
            Error::PredicateFailure => {
                write!(f, "Transaction predicate failed")
            }
            Error::Db(ref dbe) => write!(f, "Underlying DB error: {}", dbe),
        }
    }
}

impl<T> From<DbError<T>> for Error {
    #[inline]
    fn from(db_error: DbError<T>) -> Error {
        Error::Db(db_error.danger_cast())
    }
}

struct VersionedChain {
    initial_visible: Ts,
    chain: Arc<Chain>,
}

pub struct Tx<'a> {
    pub(super) db: &'a Db,
    pub(super) predicates: Vec<Predicate>,
    pub(super) sets: Vec<Write>,
    base_ts: Ts,
    chains: HashMap<Key, VersionedChain>,
}

impl<'a> Tx<'a> {
    pub(super) fn new(db: &'a Db) -> Tx<'a> {
        Tx {
            db: db,
            predicates: vec![],
            sets: vec![],
            base_ts: 0,
            chains: HashMap::new(),
        }
    }

    pub fn set(&mut self, k: Key, v: Option<Value>) {
        self.sets.push(Write(k, v));
    }

    pub fn predicate(&mut self, k: Key, p: PredicateFn) {
        self.predicates.push(Predicate(k, Box::new(p)));
    }

    pub fn execute(mut self) -> TxResult {
        // allocate timestamps for txn and versions
        self.base_ts = self.db.ts(self.sets.len());

        let res = self._execute();

        if res.is_err() {
            self.rollback()?;
        }

        res
    }

    fn _execute(&mut self) -> TxResult {
        self.version_search()?;
        self.validation()?;
        self.write()?;
        self.commit();
        self.maintenance();

        Ok(())
    }

    fn commit(&mut self) {
        for &Write(ref k, _) in &self.sets {
            let versioned_chain = self.chains.get(k).unwrap();
            versioned_chain.chain.commit(self.base_ts);
        }
    }

    fn rollback(&mut self) -> TxResult {
        for &Write(ref k, _) in &self.sets {
            let versioned_chain = self.chains.get(k).unwrap();
            versioned_chain.chain.abort(self.base_ts);

            self.db.purge_version_from_key(k, self.base_ts)?;

            // Abort all Pending in chains
            unimplemented!("abort chain, add dropper to async clean it up");
        }

        // remove the writeset
        let mut writeset_k = vec![b'!' as u8; 9];
        writeset_k[1..9].copy_from_slice(&*ts_to_bytes(self.base_ts));
        self.db.tree.del(&*writeset_k).map(|_| ()).map_err(
            |e| e.into(),
        )
    }

    fn version_search(&mut self) -> TxResult {
        let mut keyset = HashSet::new();

        for &Predicate(ref k, _) in &self.predicates {
            keyset.insert(k.clone());
        }

        for &Write(ref k, _) in &self.sets {
            keyset.insert(k.clone());
        }

        for key in keyset.into_iter() {
            // pull in chains, block if pending && wts < t.ts
            let chain = self.db.get_chain(&key).unwrap();
            let last_ts = chain.visible_ts(self.base_ts);
            if last_ts > self.base_ts {
                // abort if any wts > t.ts
                return Err(Error::Abort);
            }
            self.chains.insert(
                key,
                VersionedChain {
                    initial_visible: last_ts,
                    chain: chain,
                },
            );
        }

        Ok(())
    }

    fn validation(&mut self) -> TxResult {
        self.install_pending()?;

        self.update_read_ts()?;

        // perform predicate matches

        /* TODO
        for &Predicate(ref k, ref p) in &self.predicates {
            unimplemented!();
            let current = self.db.tree.get(k)?;
            if !p(&k, &current) {
                return Err(Error::PredicateFailure);
            }
        }
        */

        self.check_version_consistency()?;

        Ok(())
    }

    fn install_pending(&mut self) -> TxResult {
        // install pending into chain
        for (i, &Write(ref k, _)) in self.sets.iter().enumerate() {
            let versioned_chain = self.chains.get(k).unwrap();
            let version = self.base_ts + i as Ts;

            let pending = MemRecord {
                rts: AtomicUsize::new(0),
                wts: self.base_ts,
                data: Some(version),
                status: Status::Pending,
            };

            versioned_chain.chain.install(
                versioned_chain.initial_visible,
                pending,
            )?;
        }

        Ok(())
    }

    fn update_read_ts(&mut self) -> TxResult {
        for &Predicate(ref k, _) in &self.predicates {
            let versioned_chain = self.chains.get(k).unwrap();
            versioned_chain.chain.bump_rts(self.base_ts);
        }

        Ok(())
    }

    fn check_version_consistency(&mut self) -> TxResult {
        // go back to all things we read and ensure the initial visible
        // version is still visible
        for &Predicate(ref k, _) in &self.predicates {
            let versioned_chain = self.chains.get(k).unwrap();
            let last_ts = versioned_chain.chain.visible_ts(self.base_ts);
            if last_ts != versioned_chain.initial_visible {
                return Err(Error::Abort);
            }
        }

        Ok(())
    }

    fn write(&mut self) -> TxResult {
        // put writeset into Tree
        let writeset: Vec<Key> =
            self.sets.iter().map(|p| p.0.clone()).collect();
        let writeset_bytes = serialize(&writeset, Infinite).unwrap();

        let mut writeset_k = vec![b'!' as u8; 9];
        writeset_k[1..9].copy_from_slice(&*ts_to_bytes(self.base_ts));

        self.db.tree.set(writeset_k.clone(), writeset_bytes)?;

        // put versions into Tree
        for (i, &Write(ref k, ref v)) in self.sets.iter().enumerate() {
            let version = self.base_ts + i as Ts;
            let key = ts_to_bytes(version);
            if let &Some(ref value) = v {
                self.db.tree.set(key, value.clone())?;
            } else {
                unimplemented!("deletes are not yet supported");
            }

            // cas @Key to refer to new writes for each write
            self.db.add_version_to_key(k, self.base_ts, version)?;
        }

        // NB remove writeset from disk, this is the linearizing point
        // of the entire transaction as far as recovery is concerned!
        self.db.tree.del(&writeset_k)?;

        Ok(())
    }

    fn maintenance(&mut self) {
        unimplemented!();
        // put (@k, wts, version) for last good version into epoch dropper
    }
}