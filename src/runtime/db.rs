use std::cmp::Ordering::Greater;
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use either::{Left, Right};
use itertools::Itertools;
use log::debug;
use miette::{bail, ensure, Diagnostic, Result, WrapErr};
use serde_json::json;
use smartstring::SmartString;
use thiserror::Error;

use cozorocks::CfHandle::{Pri, Snd};
use cozorocks::{DbBuilder, DbIter, RocksDb};

use crate::data::json::JsonValue;
use crate::data::program::{InputProgram, QueryAssertion, RelationOp};
use crate::data::symb::Symbol;
use crate::data::tuple::{
    compare_tuple_keys, rusty_scratch_cmp, EncodedTuple, Tuple, SCRATCH_DB_KEY_PREFIX_LEN,
};
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::parse::sys::{CompactTarget, SysOp};
use crate::parse::{parse_script, CozoScript, SourceSpan};
use crate::runtime::relation::{RelationHandle, RelationId};
use crate::runtime::transact::SessionTx;
use crate::utils::swap_option_result;

struct RunningQueryHandle {
    started_at: f64,
    poison: Poison,
}

struct RunningQueryCleanup {
    id: u64,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
}

impl Drop for RunningQueryCleanup {
    fn drop(&mut self) {
        let mut map = self.running_queries.lock().unwrap();
        if let Some(handle) = map.remove(&self.id) {
            handle.poison.0.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct DbManifest {
    storage_version: u64,
}

const CURRENT_STORAGE_VERSION: u64 = 1;

pub struct Db {
    db: RocksDb,
    relation_store_id: Arc<AtomicU64>,
    n_sessions: Arc<AtomicUsize>,
    queries_count: Arc<AtomicU64>,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
    session_id: usize,
}

impl Debug for Db {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Db<session {}, sessions: {:?}>",
            self.session_id, self.n_sessions
        )
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Initialization of database failed")]
#[diagnostic(code(db::init))]
struct BadDbInit(#[help] String);

impl Db {
    pub fn build(builder: DbBuilder<'_>) -> Result<Self> {
        let path = builder.opts.db_path;
        fs::create_dir_all(path)
            .map_err(|err| BadDbInit(format!("cannot create directory {}: {}", path, err)))?;
        let path_buf = PathBuf::from(path);

        let is_new = {
            let mut manifest_path = path_buf.clone();
            manifest_path.push("manifest");

            if manifest_path.exists() {
                let existing: DbManifest = rmp_serde::from_slice(
                    &fs::read(manifest_path).expect("reading manifest failed"),
                )
                .expect("parsing manifest failed");
                assert_eq!(
                    existing.storage_version, CURRENT_STORAGE_VERSION,
                    "Unknown storage version {}",
                    existing.storage_version
                );
                false
            } else {
                fs::write(
                    manifest_path,
                    rmp_serde::to_vec_named(&DbManifest {
                        storage_version: CURRENT_STORAGE_VERSION,
                    })
                    .expect("serializing manifest failed"),
                )
                .expect("Writing to manifest failed");
                true
            }
        };

        let mut store_path = path_buf;
        store_path.push("data");
        let db_builder = builder
            .create_if_missing(is_new)
            // TODO
            .pri_use_capped_prefix_extractor(true, SCRATCH_DB_KEY_PREFIX_LEN)
            .pri_use_custom_comparator("cozo_rusty_cmp", rusty_scratch_cmp, false)
            .use_bloom_filter(true, 9.9, true)
            .snd_use_capped_prefix_extractor(true, SCRATCH_DB_KEY_PREFIX_LEN)
            .snd_use_custom_comparator("cozo_rusty_scratch_cmp", rusty_scratch_cmp, false)
            .path(store_path.to_str().unwrap());

        let db = db_builder.build()?;

        let ret = Self {
            db,
            relation_store_id: Arc::new(Default::default()),
            n_sessions: Arc::new(Default::default()),
            queries_count: Arc::new(Default::default()),
            running_queries: Arc::new(Mutex::new(Default::default())),
            session_id: Default::default(),
        };
        ret.load_last_ids()?;
        Ok(ret)
    }

    pub fn compact_relation(&self) -> Result<()> {
        let l = Tuple::default().encode_as_key(RelationId(0));
        let u = Tuple(vec![DataValue::Bot]).encode_as_key(RelationId(u64::MAX));
        self.db.range_compact(&l, &u, Snd)?;
        Ok(())
    }

    pub fn new_session(&self) -> Result<Self> {
        let old_count = self.n_sessions.fetch_add(1, Ordering::AcqRel);

        Ok(Self {
            db: self.db.clone(),
            relation_store_id: self.relation_store_id.clone(),
            n_sessions: self.n_sessions.clone(),
            queries_count: self.queries_count.clone(),
            running_queries: self.running_queries.clone(),
            session_id: old_count + 1,
        })
    }

    fn load_last_ids(&self) -> Result<()> {
        let tx = self.transact()?;
        self.relation_store_id
            .store(tx.load_last_relation_store_id()?.0, Ordering::Release);
        Ok(())
    }
    pub fn transact(&self) -> Result<SessionTx> {
        let ret = SessionTx {
            tx: self.db.transact().set_snapshot(true).start(),
            mem_store_id: Default::default(),
            relation_store_id: self.relation_store_id.clone(),
        };
        Ok(ret)
    }
    pub fn transact_write(&self) -> Result<SessionTx> {
        let ret = SessionTx {
            tx: self.db.transact().set_snapshot(true).start(),
            mem_store_id: Default::default(),
            relation_store_id: self.relation_store_id.clone(),
        };
        Ok(ret)
    }
    pub fn total_iter(&self) -> DbIter {
        let mut it = self.db.transact().start().iterator(Pri).start();
        it.seek_to_start();
        it
    }
    pub fn run_script(
        &self,
        payload: &str,
        params: &BTreeMap<String, JsonValue>,
    ) -> Result<JsonValue> {
        self.do_run_script(payload, params).map_err(|err| {
            if err.source_code().is_some() {
                err
            } else {
                err.with_source_code(payload.to_string())
            }
        })
    }
    fn do_run_script(
        &self,
        payload: &str,
        params: &BTreeMap<String, JsonValue>,
    ) -> Result<JsonValue> {
        let param_pool = params
            .iter()
            .map(|(k, v)| (k.clone(), DataValue::from(v)))
            .collect();
        match parse_script(payload, &param_pool)? {
            CozoScript::Multi(ps) => {
                let is_write = ps.iter().any(|p| p.out_opts.store_relation.is_some());
                let mut tx = if is_write {
                    self.transact_write()?
                } else {
                    self.transact()?
                };
                let mut res = json!(null);
                let mut cleanups = vec![];
                for p in ps {
                    let (q_res, q_cleanups) = self.run_query(&mut tx, p)?;
                    res = q_res;
                    cleanups.extend(q_cleanups);
                }
                if is_write {
                    tx.commit_tx()?;
                } else {
                    assert!(cleanups.is_empty(), "non-empty cleanups on read-only tx");
                }
                for (lower, upper) in cleanups {
                    self.db.range_del(&lower, &upper, Snd)?;
                }
                Ok(res)
            }
            CozoScript::Sys(op) => self.run_sys_op(op),
        }
    }
    fn run_sys_op(&self, op: SysOp) -> Result<JsonValue> {
        match op {
            SysOp::Compact(opts) => {
                for opt in opts {
                    match opt {
                        CompactTarget::Relations => {
                            self.compact_relation()?;
                        }
                    }
                }
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRelations => self.list_relations(),
            SysOp::RemoveRelation(rs) => {
                self.remove_relation(&rs)?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRelation(rs) => self.list_relation(&rs),
            SysOp::RenameRelation(old, new) => {
                let mut tx = self.transact_write()?;
                tx.rename_relation(old, new)?;
                tx.commit_tx()?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRunning => self.list_running(),
            SysOp::KillRunning(id) => {
                let queries = self.running_queries.lock().unwrap();
                Ok(match queries.get(&id) {
                    None => {
                        json!({"headers": ["status"], "rows": [["NOT_FOUND"]]})
                    }
                    Some(handle) => {
                        handle.poison.0.store(true, Ordering::Relaxed);
                        json!({"headers": ["status"], "rows": [["KILLING"]]})
                    }
                })
            }
        }
    }
    fn run_query(
        &self,
        tx: &mut SessionTx,
        input_program: InputProgram,
    ) -> Result<(JsonValue, Vec<(Vec<u8>, Vec<u8>)>)> {
        let mut clean_ups = vec![];
        if let Some((meta, op)) = &input_program.out_opts.store_relation {
            if *op == RelationOp::Create {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} conflicts with an existing one")]
                #[diagnostic(code(eval::stored_relation_conflict))]
                struct StoreRelationConflict(String);

                ensure!(
                    !tx.relation_exists(&meta.name)?,
                    StoreRelationConflict(meta.name.to_string())
                )
            } else if *op != RelationOp::ReDerive {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} not found")]
                #[diagnostic(code(eval::stored_relation_not_found))]
                struct StoreRelationNotFoundError(String);

                let existing = tx.get_relation(&meta.name)?;

                ensure!(
                    tx.relation_exists(&meta.name)?,
                    StoreRelationNotFoundError(meta.name.to_string())
                );

                existing.ensure_compatible(meta)?;
            }
        };
        let program = input_program
            .to_normalized_program(tx)?
            .stratify()?
            .magic_sets_rewrite()?;
        debug!("{:#?}", program);
        let (compiled, stores) =
            tx.stratified_magic_compile(&program, &input_program.const_rules)?;

        let poison = Poison::default();
        if let Some(secs) = input_program.out_opts.timeout {
            poison.set_timeout(secs);
        }
        let id = self.queries_count.fetch_add(1, Ordering::AcqRel);

        let now = SystemTime::now();
        let since_the_epoch = now
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs_f64();

        let handle = RunningQueryHandle {
            started_at: since_the_epoch,
            poison: poison.clone(),
        };
        self.running_queries.lock().unwrap().insert(id, handle);
        let _guard = RunningQueryCleanup {
            id,
            running_queries: self.running_queries.clone(),
        };

        let result = tx.stratified_magic_evaluate(
            &compiled,
            &stores,
            if input_program.out_opts.sorters.is_empty() {
                input_program.out_opts.num_to_take()
            } else {
                None
            },
            poison,
        )?;
        if let Some(assertion) = &input_program.out_opts.assertion {
            match assertion {
                QueryAssertion::AssertNone(span) => {
                    if let Some(tuple) = result.scan_all().next() {
                        let tuple = tuple?;

                        #[derive(Debug, Error, Diagnostic)]
                        #[error(
                            "The query is asserted to return no result, but a tuple {0:?} is found"
                        )]
                        #[diagnostic(code(eval::assert_none_failure))]
                        struct AssertNoneFailure(Tuple, #[label] SourceSpan);
                        bail!(AssertNoneFailure(tuple, *span))
                    }
                }
                QueryAssertion::AssertSome(span) => {
                    if let Some(tuple) = result.scan_all().next() {
                        let _ = tuple?;
                    } else {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error("The query is asserted to return some results, but returned none")]
                        #[diagnostic(code(eval::assert_some_failure))]
                        struct AssertSomeFailure(#[label] SourceSpan);
                        bail!(AssertSomeFailure(*span))
                    }
                }
            }
        }
        let json_headers = match input_program.get_entry_out_head() {
            Err(_) => JsonValue::Null,
            Ok(headers) => headers.into_iter().map(|v| json!(v.name)).collect(),
        };
        if !input_program.out_opts.sorters.is_empty() {
            let entry_head = input_program.get_entry_out_head()?;
            let sorted_result =
                tx.sort_and_collect(result, &input_program.out_opts.sorters, &entry_head)?;
            let sorted_iter = if let Some(offset) = input_program.out_opts.offset {
                Left(sorted_result.scan_sorted().skip(offset))
            } else {
                Right(sorted_result.scan_sorted())
            };
            let sorted_iter = if let Some(limit) = input_program.out_opts.limit {
                Left(sorted_iter.take(limit))
            } else {
                Right(sorted_iter)
            };
            if let Some((meta, relation_op)) = &input_program.out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        sorted_iter,
                        *relation_op,
                        &meta,
                        &input_program.get_entry_out_head_or_default()?,
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                if let Some(c) = to_clear {
                    clean_ups.push(c);
                }
                Ok((json!({"headers": ["status"], "rows": [["OK"]]}), clean_ups))
            } else {
                let ret: Vec<Vec<JsonValue>> = sorted_iter
                    .map_ok(|tuple| -> Vec<JsonValue> {
                        tuple.0.into_iter().map(JsonValue::from).collect()
                    })
                    .try_collect()?;

                Ok((json!({ "rows": ret, "headers": json_headers }), clean_ups))
            }
        } else {
            let scan = if input_program.out_opts.limit.is_some()
                || input_program.out_opts.offset.is_some()
            {
                let limit = input_program.out_opts.limit.unwrap_or(usize::MAX);
                let offset = input_program.out_opts.offset.unwrap_or(0);
                Right(result.scan_all().skip(offset).take(limit))
            } else {
                Left(result.scan_all())
            };

            if let Some((meta, relation_op)) = &input_program.out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        scan,
                        *relation_op,
                        &meta,
                        &input_program.get_entry_out_head_or_default()?,
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                if let Some(c) = to_clear {
                    clean_ups.push(c);
                }
                Ok((json!({"headers": ["status"], "rows": [["OK"]]}), clean_ups))
            } else {
                let ret: Vec<Vec<JsonValue>> = scan
                    .map_ok(|tuple| -> Vec<JsonValue> {
                        tuple.0.into_iter().map(JsonValue::from).collect()
                    })
                    .try_collect()?;

                Ok((json!({ "rows": ret, "headers": json_headers }), clean_ups))
            }
        }
    }
    pub(crate) fn remove_relation(&self, name: &Symbol) -> Result<()> {
        let mut tx = self.transact_write()?;
        let (lower, upper) = tx.destroy_relation(name)?;
        tx.commit_tx()?;
        self.db.range_del(&lower, &upper, Snd)?;
        Ok(())
    }
    pub(crate) fn list_running(&self) -> Result<JsonValue> {
        let res = self
            .running_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| json!([k, format!("{:?}", v.started_at)]))
            .collect_vec();
        Ok(json!({"rows": res, "headers": ["id", "started_at"]}))
    }
    pub fn put_meta_kv(&self, k: &[&str], v: &[u8]) -> Result<()> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(RelationId::SYSTEM);
        let mut vtx = self.db.transact().start();
        vtx.put(&key, v, Snd)?;
        vtx.commit()?;
        Ok(())
    }
    pub fn remove_meta_kv(&self, k: &[&str]) -> Result<()> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(RelationId::SYSTEM);
        let mut vtx = self.db.transact().start();
        vtx.del(&key, Snd)?;
        vtx.commit()?;
        Ok(())
    }
    pub fn get_meta_kv(&self, k: &[&str]) -> Result<Option<Vec<u8>>> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(RelationId::SYSTEM);
        let vtx = self.db.transact().start();
        Ok(vtx.get(&key, false, Snd)?.map(|slice| slice.to_vec()))
    }
    pub fn meta_range_scan(
        &self,
        prefix: &[&str],
    ) -> impl Iterator<Item = Result<(Vec<String>, Vec<u8>)>> {
        let mut lower_bound = Tuple(vec![DataValue::Guard]);
        for p in prefix {
            lower_bound.0.push(DataValue::Str(SmartString::from(*p)));
        }
        let upper_data_bound = Tuple(vec![DataValue::Bot]);
        let upper_bound = upper_data_bound.encode_as_key(RelationId::SYSTEM);
        let mut it = self
            .db
            .transact()
            .start()
            .iterator(Snd)
            .upper_bound(&upper_bound)
            .start();
        it.seek(&lower_bound.encode_as_key(RelationId::SYSTEM));

        struct CustomIter {
            it: DbIter,
            started: bool,
            upper_bound: Vec<u8>,
        }

        impl CustomIter {
            fn next_inner(&mut self) -> Result<Option<(Vec<String>, Vec<u8>)>> {
                if self.started {
                    self.it.next()
                } else {
                    self.started = true;
                }
                match self.it.pair()? {
                    None => Ok(None),
                    Some((k_slice, v_slice)) => {
                        if compare_tuple_keys(&self.upper_bound, k_slice) != Greater {
                            return Ok(None);
                        }

                        #[derive(Debug, Error, Diagnostic)]
                        #[error("Encountered corrupt key in meta store")]
                        #[diagnostic(code(db::corrupt_meta_key))]
                        #[diagnostic(help("This is an internal error. Please file a bug."))]
                        struct CorruptKeyInMetaStoreError;

                        let encoded = EncodedTuple(k_slice).decode();
                        let ks: Vec<_> = encoded
                            .0
                            .into_iter()
                            .skip(1)
                            .map(|v| {
                                v.get_string()
                                    .map(|s| s.to_string())
                                    .ok_or(CorruptKeyInMetaStoreError)
                            })
                            .try_collect()?;
                        Ok(Some((ks, v_slice.to_vec())))
                    }
                }
            }
        }

        impl Iterator for CustomIter {
            type Item = Result<(Vec<String>, Vec<u8>)>;

            fn next(&mut self) -> Option<Self::Item> {
                swap_option_result(self.next_inner())
            }
        }

        CustomIter {
            it,
            started: false,
            upper_bound,
        }
    }
    pub fn list_relation(&self, name: &str) -> Result<JsonValue> {
        let tx = self.transact()?;
        let handle = tx.get_relation(name)?;
        let mut ret = vec![];
        let mut idx = 0;
        for col in &handle.metadata.keys {
            ret.push(json!([
                col.name,
                true,
                idx,
                col.typing.to_string(),
                col.default_gen.is_some()
            ]));
            idx += 1;
        }
        for col in &handle.metadata.dependents {
            ret.push(json!([
                col.name,
                false,
                idx,
                col.typing.to_string(),
                col.default_gen.is_some()
            ]));
            idx += 1;
        }
        Ok(json!({"rows": ret, "headers": ["column", "is_key", "index", "type", "has_default"]}))
    }
    pub fn list_relations(&self) -> Result<JsonValue> {
        let lower =
            Tuple(vec![DataValue::Str(SmartString::from(""))]).encode_as_key(RelationId::SYSTEM);
        let upper = Tuple(vec![DataValue::Str(SmartString::from(String::from(
            LARGEST_UTF_CHAR,
        )))])
        .encode_as_key(RelationId::SYSTEM);
        let mut it = self
            .db
            .transact()
            .start()
            .iterator(Snd)
            .upper_bound(&upper)
            .start();
        it.seek(&lower);
        let mut collected = vec![];
        while let Some((k_slice, v_slice)) = it.pair()? {
            if compare_tuple_keys(&upper, k_slice) != Greater {
                break;
            }
            let meta = RelationHandle::decode(v_slice)?;
            let n_keys = meta.metadata.keys.len();
            let n_dependents = meta.metadata.dependents.len();
            let arity = n_keys + n_dependents;
            let name = meta.name;
            collected.push(json!([name, arity, n_keys, n_dependents]));
            it.next();
        }
        Ok(json!({"rows": collected, "headers": ["name", "arity", "n_keys", "n_non_keys"]}))
    }
}

#[derive(Clone, Default)]
pub(crate) struct Poison(pub(crate) Arc<AtomicBool>);

impl Poison {
    #[inline(always)]
    pub(crate) fn check(&self) -> Result<()> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("Process is killed before completion")]
        #[diagnostic(code(eval::killed))]
        #[diagnostic(help("A process may be killed by timeout, or explicit command"))]
        struct ProcessKilled;

        if self.0.load(Ordering::Relaxed) {
            bail!(ProcessKilled)
        }
        Ok(())
    }
    pub(crate) fn set_timeout(&self, secs: u64) {
        let pill = self.0.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            pill.store(true, Ordering::Relaxed);
        });
    }
}
