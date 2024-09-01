use {
    pickledb::PickleDb,
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        bpf_loader_upgradeable,
        clock::Slot,
        feature_set::FeatureSet,
        fee::FeeStructure,
        hash::Hash,
        pubkey::Pubkey,
        rent_collector::RentCollector,
        transaction,
        transaction::SanitizedTransaction,
    },
    solana_svm::{
        account_loader::CheckedTransactionDetails,
        transaction_processing_callback::TransactionProcessingCallback,
        transaction_processor::TransactionBatchProcessor,
        transaction_processor::{
            ExecutionRecordingConfig, LoadAndExecuteSanitizedTransactionsOutput,
            TransactionProcessingConfig, TransactionProcessingEnvironment,
        },
    },
    solana_system_program::system_processor,
    std::{
        collections::{HashMap, HashSet},
        sync::{Arc, RwLock},
    },
    // tokio::sync::RwLock,
};

// As there will be no standing forks as in solana with snowman consensus,
// this implementation can be mocked.
pub(crate) struct EscobarForkGraph {}

impl ForkGraph for EscobarForkGraph {
    fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
        BlockRelation::Unknown
    }
}

pub(crate) fn create_transaction_batch_processor<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    feature_set: &FeatureSet,
    compute_budget: &ComputeBudget,
    fork_graph: Arc<RwLock<EscobarForkGraph>>,
    slot: Slot, // slot and epoch are set same for escobar.
) -> TransactionBatchProcessor<EscobarForkGraph> {
    let processor = TransactionBatchProcessor::<EscobarForkGraph>::new(slot, slot, HashSet::new());
    {
        let mut cache = processor.program_cache.write().unwrap();

        // Initialize the mocked fork graph.
        // let fork_graph = Arc::new(RwLock::new(PayTubeForkGraph {}));
        cache.fork_graph = Some(Arc::downgrade(&fork_graph));

        // Initialize a proper cache environment.
        // (Use Loader v4 program to initialize runtime v2 if desired)
        cache.environments.program_runtime_v1 = Arc::new(
            create_program_runtime_environment_v1(feature_set, compute_budget, false, false)
                .unwrap(),
        );
    }

    // Add the system program builtin.
    processor.add_builtin(
        callbacks,
        solana_system_program::id(),
        "system_program",
        ProgramCacheEntry::new_builtin(
            0,
            b"system_program".len(),
            system_processor::Entrypoint::vm,
        ),
    );

    // Add the BPF Loader v2 builtin, for the SPL Token program.
    processor.add_builtin(
        callbacks,
        solana_sdk::bpf_loader::id(),
        "solana_bpf_loader_program",
        ProgramCacheEntry::new_builtin(
            0,
            b"solana_bpf_loader_program".len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );

    // Add the BPF Loader Upgradeable builtin, for programs.
    processor.add_builtin(
        callbacks,
        bpf_loader_upgradeable::id(),
        "solana_bpf_loader_upgradeable_program",
        ProgramCacheEntry::new_builtin(
            0,
            b"solana_bpf_loader_upgradeable_program".len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );
    processor
}

/// We are not performing any signature verification here. Signatures are verified independently to parts of svm.
pub(crate) fn get_transaction_check_results(
    len: usize,
    lamports_per_signature: u64,
) -> Vec<transaction::Result<CheckedTransactionDetails>> {
    vec![
        transaction::Result::Ok(CheckedTransactionDetails {
            nonce: None,
            lamports_per_signature,
        });
        len
    ]
}

#[derive(Clone)]
pub struct EscobarDB {
    pub db: Arc<RwLock<PickleDb>>,
    pub cache: Arc<RwLock<HashMap<Pubkey, AccountSharedData>>>,
}

impl Default for EscobarDB {
    fn default() -> Self {
        Self {
            db: Arc::new(RwLock::new(PickleDb::new(
                "state.db",
                pickledb::PickleDbDumpPolicy::AutoDump,
                pickledb::SerializationMethod::Json,
            ))),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl EscobarDB {
    pub fn new(data_dir: String) -> Self {
        Self {
            db: Arc::new(RwLock::new(PickleDb::new(
                data_dir + "/state.db",
                pickledb::PickleDbDumpPolicy::AutoDump,
                pickledb::SerializationMethod::Json,
            ))),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub fn write(&self, key: Pubkey, value: AccountSharedData) {
        self.db
            .write()
            .unwrap()
            .set(&key.to_string(), &value)
            .unwrap();
        self.cache.write().unwrap().insert(key, value);
    }

    pub fn write_batch(&self, accounts: Vec<(&Pubkey, &AccountSharedData)>) {
        let mut db = self.db.write().unwrap();
        let mut cache = self.cache.write().unwrap();
        for (key, value) in accounts {
            db.set(&key.to_string(), value).unwrap();
            cache.insert(*key, value.clone());
        }
    }

    pub fn get(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        if let Some(account) = self.cache.read().unwrap().get(pubkey) {
            return Some(account.clone());
        }
        let account: AccountSharedData = self.db.read().unwrap().get(&pubkey.to_string()).unwrap();
        self.cache.write().unwrap().insert(*pubkey, account.clone());
        Some(account)
    }
}

impl TransactionProcessingCallback for EscobarDB {
    fn get_account_shared_data(
        &self,
        pubkey: &Pubkey,
    ) -> Option<solana_sdk::account::AccountSharedData> {
        if let Some(account) = self.cache.read().unwrap().get(pubkey) {
            return Some(account.clone());
        }
        let account: AccountSharedData = self.db.read().unwrap().get(&pubkey.to_string()).unwrap();
        self.cache.write().unwrap().insert(*pubkey, account.clone());
        Some(account)
    }

    fn account_matches_owners(
        &self,
        account: &solana_sdk::pubkey::Pubkey,
        owners: &[solana_sdk::pubkey::Pubkey],
    ) -> Option<usize> {
        self.get_account_shared_data(account)
            .and_then(|account| owners.iter().position(|key| account.owner().eq(key)))
    }
}

pub fn execute_transactions<T: TransactionProcessingCallback>(
    slot: Slot,
    cb: T,
    transactions: Vec<SanitizedTransaction>,
) -> LoadAndExecuteSanitizedTransactionsOutput {
    let compute_budget = ComputeBudget::default();
    let feature_set = FeatureSet::all_enabled();
    let fee_structure = FeeStructure::default();
    let lamports_per_signature = fee_structure.lamports_per_signature;
    let rent_collector = RentCollector::default();

    let fork_graph = Arc::new(RwLock::new(EscobarForkGraph {}));

    let processor =
        create_transaction_batch_processor(&cb, &feature_set, &compute_budget, fork_graph, slot);

    let processing_environment = TransactionProcessingEnvironment {
        blockhash: Hash::default(),
        epoch_total_stake: None,
        epoch_vote_accounts: None,
        feature_set: Arc::new(feature_set),
        fee_structure: Some(&fee_structure),
        lamports_per_signature,
        rent_collector: Some(&rent_collector),
    };

    let processing_config = TransactionProcessingConfig {
        compute_budget: Some(compute_budget),
        recording_config: ExecutionRecordingConfig {
            enable_log_recording: true,
            enable_return_data_recording: true,
            enable_cpi_recording: false,
        },
        ..Default::default()
    };

    let check_results = get_transaction_check_results(transactions.len(), lamports_per_signature);

    processor.load_and_execute_sanitized_transactions(
        &cb,
        &transactions,
        check_results,
        &processing_environment,
        &processing_config,
    )
}

#[cfg(test)]
mod processor_test {
    use std::vec;

    use crate::processor::EscobarDB;
    use solana_sdk::account::{AccountSharedData, WritableAccount};
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_serialize_and_deserialize_account_shared_data_to_db() {
        let db = EscobarDB::default();
        let account = AccountSharedData::create(1000, vec![1, 2, 3], Pubkey::new_unique(), true, 1);
        // account.data_as_mut_slice()
        // println!("{:?}", account);
        let key = Pubkey::new_unique();
        db.write(key, account.clone());
        // db.get_account_shared_data(pubkey)
        let account2 = db.get(&key).unwrap();
        assert_eq!(account, account2);
    }
}
