/*
 * Copyright 2018 Intel Corporation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * ------------------------------------------------------------------------------
 */

#![allow(unknown_lints)]

use std::collections::HashSet;

use cpython;
use cpython::ObjectProtocol;
use cpython::PyClone;
use cpython::PyList;
use cpython::Python;

use sawtooth::hashlib::sha256_digest_strs;
use sawtooth::journal::candidate_block::{
    CandidateBlock, CandidateBlockError, FinalizeBlockResult,
};
use sawtooth::journal::chain_commit_state::TransactionCommitCache;
use sawtooth::journal::commit_store::CommitStore;
use sawtooth::journal::validation_rule_enforcer::{
    ValidationRuleEnforcer, ValidationRuleEnforcerError,
};
use sawtooth::protocol::block::BlockPair;
use sawtooth::scheduler::Scheduler;
use sawtooth::state::settings_view::SettingsView;
use transact::protocol::{batch::Batch, transaction::Transaction};

use crate::py_object_wrapper::PyObjectWrapper;

use pylogger;

pub struct FFICandidateBlock {
    previous_block: BlockPair,
    commit_store: CommitStore,
    scheduler: Box<dyn Scheduler>,
    max_batches: usize,
    block_builder: cpython::PyObject,
    batch_injectors: Vec<cpython::PyObject>,
    identity_signer: cpython::PyObject,
    settings_view: SettingsView,

    summary: Option<Vec<u8>>,
    /// Batches remaining after the summary has been computed
    remaining_batches: Vec<Batch>,

    pending_batches: Vec<Batch>,
    pending_batch_ids: HashSet<String>,
    injected_batch_ids: HashSet<String>,

    committed_txn_cache: TransactionCommitCache,
}

impl CandidateBlock for FFICandidateBlock {
    fn cancel(&mut self) {
        self.scheduler.cancel().unwrap();
    }

    fn previous_block_id(&self) -> String {
        self.previous_block.block().header_signature().to_string()
    }

    fn can_add_batch(&self) -> bool {
        self.summary.is_none()
            && (self.max_batches == 0 || self.pending_batches.len() < self.max_batches)
    }

    fn add_batch(&mut self, batch: Batch) {
        let batch_header_signature = batch.header_signature().to_string();

        if batch.trace() {
            debug!(
                "TRACE {}: {}",
                batch_header_signature, "FFICandidateBlock , add_batch"
            );
        }

        if self.batch_is_already_committed(&batch) {
            debug!(
                "Dropping previously committed batch: {}",
                batch_header_signature
            );
        } else if self.check_batch_dependencies_add_batch(&batch) {
            let mut batches_to_add = vec![];

            // Inject blocks at the beginning of a Candidate Block
            if self.pending_batches.is_empty() {
                let previous_block = self.previous_block.clone();
                let mut injected_batches = self.poll_injectors(|injector: &cpython::PyObject| {
                    let gil = cpython::Python::acquire_gil();
                    let py = gil.python();
                    match injector
                        .call_method(
                            py,
                            "block_start",
                            (PyObjectWrapper::from(previous_block.clone()),),
                            None,
                        )
                        .expect("BlockInjector.block_start failed")
                        .extract::<cpython::PyList>(py)
                    {
                        Ok(injected) => injected.iter(py).collect(),
                        Err(err) => {
                            pylogger::exception(
                                py,
                                "During block injection, calling block_start",
                                err,
                            );
                            vec![]
                        }
                    }
                });
                batches_to_add.append(&mut injected_batches);
            }

            batches_to_add.push(batch);

            {
                let mut batches_to_test = self.pending_batches.clone();
                batches_to_test.append(&mut batches_to_add.clone());
                let mut validation_rule_enforcer = ValidationRuleEnforcer::new(
                    &self.settings_view,
                    self.get_signer_public_key_hex(),
                )
                .expect("Unable to get ValidationRuleEnforcer");

                match validation_rule_enforcer.add_batches(&batches_to_test) {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(
                            "Block validation rules violated, rejecting batch: {}",
                            batch_header_signature
                        );
                        return;
                    }
                    Err(ValidationRuleEnforcerError::InvalidBatches(_)) => {
                        debug!("Rejecting invalid batch: {}", batch_header_signature);
                        return;
                    }
                    Err(err) => {
                        error!("Unable to validate error: {}", err.to_string());
                        return;
                    }
                };
            }

            for b in batches_to_add {
                self.pending_batches.push(b.clone());
                self.pending_batch_ids
                    .insert(b.header_signature().to_string());

                let injected = self.injected_batch_ids.contains(b.header_signature());

                self.scheduler.add_batch(b, None, injected).unwrap()
            }
        } else {
            debug!(
                "Dropping batch due to missing dependencies: {}",
                batch_header_signature
            );
        }
    }

    fn summarize(&mut self, force: bool) -> Result<Option<Vec<u8>>, CandidateBlockError> {
        if let Some(ref summary) = self.summary {
            return Ok(Some(summary.clone()));
        }

        if !force && self.pending_batches.is_empty() {
            return Err(CandidateBlockError::BlockEmpty);
        }

        self.scheduler.finalize(true).unwrap();
        let execution_results = self.scheduler.complete(true).unwrap().unwrap();

        let mut committed_txn_cache = TransactionCommitCache::new(self.commit_store.clone());

        let batches_w_no_results: Vec<String> = execution_results
            .batch_results
            .iter()
            .filter(|(_, txns)| txns.is_none())
            .map(|(batch_id, _)| batch_id.clone())
            .collect();

        let valid_batch_ids: HashSet<String> = execution_results
            .batch_results
            .into_iter()
            .filter(|(_, txns)| match txns {
                Some(t) => !t.iter().any(|t| !t.is_valid),
                None => false,
            })
            .map(|(b_id, _)| b_id)
            .collect();

        let builder = {
            let gil = Python::acquire_gil();
            let py = gil.python();
            self.block_builder.clone_ref(py)
        };

        let mut bad_batches = vec![];
        let mut pending_batches = vec![];

        if self.injected_batch_ids == valid_batch_ids {
            // There only injected batches in this block
            return Ok(None);
        }

        for batch in self.pending_batches.clone() {
            let header_signature = batch.header_signature().to_string();
            if batch.trace() {
                debug!("TRACE {} : FFICandidateBlock  finalize", header_signature)
            }

            if batches_w_no_results.contains(&header_signature) {
                if !self.injected_batch_ids.contains(&header_signature) {
                    pending_batches.push(batch)
                } else {
                    warn! {
                        "Failed to inject batch {}",
                        header_signature
                    };
                }
            } else if valid_batch_ids.contains(&header_signature) {
                if !self.check_batch_dependencies(&batch, &mut committed_txn_cache) {
                    debug!(
                        "Batch {} is invalid, due to missing txn dependency",
                        header_signature
                    );
                    bad_batches.push(batch);
                    pending_batches.clear();
                    pending_batches.append(
                        &mut self
                            .pending_batches
                            .clone()
                            .into_iter()
                            .filter(|b| !bad_batches.contains(b))
                            .collect(),
                    );
                    return Ok(None);
                } else {
                    let gil = Python::acquire_gil();
                    let py = gil.python();
                    let batch_wrapper = PyObjectWrapper::from(batch.clone());
                    builder
                        .call_method(py, "add_batch", (batch_wrapper,), None)
                        .expect("BlockBuilder has no method 'add_batch'");
                    committed_txn_cache.add_batch(&batch.clone());
                }
            } else {
                bad_batches.push(batch.clone());
                debug!("Batch {} invalid, not added to block", header_signature);
            }
        }
        if execution_results.ending_state_hash.is_none() || self.no_batches_added(&builder) {
            debug!("Abandoning block, no batches added");
            return Ok(None);
        }

        let gil = cpython::Python::acquire_gil();
        let py = gil.python();
        builder
            .call_method(
                py,
                "set_state_hash",
                (execution_results.ending_state_hash.map(hex::encode),),
                None,
            )
            .expect("BlockBuilder has no method 'set_state_hash'");

        let batch_py_objs = builder
            .getattr(py, "batches")
            .expect("BlockBuilder has no attribute 'batches'")
            .extract::<PyList>(py)
            .expect("Failed to extract PyList from uncommitted_batches")
            .iter(py)
            .map(PyObjectWrapper::new)
            .collect::<Vec<PyObjectWrapper>>();

        let batches = batch_py_objs
            .into_iter()
            .map(Batch::from)
            .collect::<Vec<Batch>>();

        let batch_ids = batches
            .iter()
            .map(|batch| batch.header_signature().to_string())
            .collect::<Vec<_>>();

        self.summary = Some(sha256_digest_strs(batch_ids.as_slice()));
        self.remaining_batches = pending_batches;

        Ok(self.summary.clone())
    }

    fn finalize(
        &mut self,
        consensus_data: &[u8],
        force: bool,
    ) -> Result<FinalizeBlockResult, CandidateBlockError> {
        let summary = if self.summary.is_none() {
            self.summarize(force)?
        } else {
            self.summary.clone()
        };
        if summary.is_none() {
            return self.build_result(None);
        }

        let builder = &self.block_builder;
        let gil = cpython::Python::acquire_gil();
        let py = gil.python();
        builder
            .getattr(py, "block_header")
            .expect("BlockBuilder has no attribute 'block_header'")
            .setattr(py, "consensus", cpython::PyBytes::new(py, consensus_data))
            .expect("BlockHeader has no attribute 'consensus'");

        self.sign_block(builder);

        self.build_result(Some(
            builder
                .call_method(py, "build_block", cpython::NoArgs, None)
                .expect("BlockBuilder has no method 'build_block'"),
        ))
    }
}

impl FFICandidateBlock {
    #![allow(clippy::too_many_arguments)]
    pub fn new(
        previous_block: BlockPair,
        commit_store: CommitStore,
        scheduler: Box<dyn Scheduler>,
        committed_txn_cache: TransactionCommitCache,
        block_builder: cpython::PyObject,
        max_batches: usize,
        batch_injectors: Vec<cpython::PyObject>,
        identity_signer: cpython::PyObject,
        settings_view: SettingsView,
    ) -> Self {
        FFICandidateBlock {
            previous_block,
            commit_store,
            scheduler,
            max_batches,
            committed_txn_cache,
            block_builder,
            batch_injectors,
            identity_signer,
            settings_view,
            summary: None,
            remaining_batches: vec![],
            pending_batches: vec![],
            pending_batch_ids: HashSet::new(),
            injected_batch_ids: HashSet::new(),
        }
    }

    pub fn last_batch(&self) -> Option<&Batch> {
        self.pending_batches.last()
    }

    fn check_batch_dependencies_add_batch(&mut self, batch: &Batch) -> bool {
        for txn in batch.transactions() {
            if self.txn_is_already_committed(txn, &self.committed_txn_cache) {
                debug!(
                    "Transaction rejected as it is already in the chain {}",
                    txn.header_signature()
                );
                return false;
            } else if !self.check_transaction_dependencies(txn) {
                self.committed_txn_cache.remove_batch(batch);
                return false;
            }
            self.committed_txn_cache
                .add(txn.header_signature().to_string());
        }
        true
    }

    fn check_batch_dependencies(
        &mut self,
        batch: &Batch,
        committed_txn_cache: &mut TransactionCommitCache,
    ) -> bool {
        for txn in batch.transactions() {
            if self.txn_is_already_committed(txn, committed_txn_cache) {
                debug!(
                    "Transaction rejected as it is already in the chain {}",
                    txn.header_signature()
                );
                return false;
            } else if !self.check_transaction_dependencies(txn) {
                committed_txn_cache.remove_batch(batch);
                return false;
            }
            committed_txn_cache.add(txn.header_signature().to_string());
        }
        true
    }

    fn check_transaction_dependencies(&self, txn: &Transaction) -> bool {
        let txn_header = match txn.clone().into_pair() {
            Ok(txn_pair) => txn_pair.take().1,
            Err(err) => {
                debug!(
                    "Transaction rejected, unable to parse transaction header: {}",
                    err
                );
                return false;
            }
        };
        for dep in txn_header.dependencies() {
            let dep = hex::encode(dep);
            if !self.committed_txn_cache.contains(&dep) {
                debug!(
                    "Transaction rejected due to missing dependency, transaction {} depends on {}",
                    txn.header_signature(),
                    dep
                );
                return false;
            }
        }
        true
    }

    fn txn_is_already_committed(
        &self,
        txn: &Transaction,
        committed_txn_cache: &TransactionCommitCache,
    ) -> bool {
        committed_txn_cache.contains(txn.header_signature())
            || self
                .commit_store
                .contains_transaction(txn.header_signature())
                .expect("Couldn't check for txn")
    }

    fn batch_is_already_committed(&self, batch: &Batch) -> bool {
        self.pending_batch_ids.contains(batch.header_signature())
            || self
                .commit_store
                .contains_batch(batch.header_signature())
                .expect("Couldn't check for batch")
    }

    fn poll_injectors<F: Fn(&cpython::PyObject) -> Vec<cpython::PyObject>>(
        &mut self,
        poller: F,
    ) -> Vec<Batch> {
        let mut batches = vec![];
        for injector in &self.batch_injectors {
            let inject_list = poller(injector);
            if !inject_list.is_empty() {
                for b in inject_list {
                    let py_wrapper = PyObjectWrapper::new(b);
                    let batch = Batch::from(py_wrapper);
                    self.injected_batch_ids
                        .insert(batch.header_signature().to_string());
                    batches.push(batch);
                }
            }
        }
        batches
    }

    fn get_signer_public_key_hex(&self) -> Vec<u8> {
        let gil = cpython::Python::acquire_gil();
        let py = gil.python();

        self.identity_signer
            .call_method(py, "get_public_key", cpython::NoArgs, None)
            .expect("IdentitySigner has no method 'get_public_key'")
            .call_method(py, "as_bytes", cpython::NoArgs, None)
            .expect("PublicKey has no method 'as_bytes'")
            .extract(py)
            .expect("Unable to convert python bytes to rust")
    }

    pub fn sign_block(&self, block_builder: &cpython::PyObject) {
        let gil = cpython::Python::acquire_gil();
        let py = gil.python();
        let header_bytes = block_builder
            .getattr(py, "block_header")
            .expect("BlockBuilder has no attribute 'block_header'")
            .call_method(py, "SerializeToString", cpython::NoArgs, None)
            .unwrap();
        let signature = self
            .identity_signer
            .call_method(py, "sign", (header_bytes,), None)
            .expect("Signer has no method 'sign'");
        block_builder
            .call_method(py, "set_signature", (signature,), None)
            .expect("BlockBuilder has no method 'set_signature'");
    }

    fn no_batches_added(&self, builder: &cpython::PyObject) -> bool {
        let gil = cpython::Python::acquire_gil();
        let py = gil.python();
        builder
            .getattr(py, "batches")
            .expect("BlockBuilder has no attribute 'batches'")
            .extract::<cpython::PyList>(py)
            .unwrap()
            .len(py)
            == 0
    }

    fn build_result(
        &self,
        block: Option<cpython::PyObject>,
    ) -> Result<FinalizeBlockResult, CandidateBlockError> {
        if let Some(last_batch) = self.last_batch().cloned() {
            let block = block.map(|py_block| BlockPair::from(PyObjectWrapper::new(py_block)));

            Ok(FinalizeBlockResult {
                block,
                remaining_batches: self.remaining_batches.clone(),
                last_batch,
                injected_batch_ids: self
                    .injected_batch_ids
                    .clone()
                    .into_iter()
                    .collect::<Vec<String>>(),
            })
        } else {
            Err(CandidateBlockError::BlockEmpty)
        }
    }
}
