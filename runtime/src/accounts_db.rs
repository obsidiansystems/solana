//! Persistent accounts are stored in below path location:
//!  <path>/<pid>/data/
//!
//! The persistent store would allow for this mode of operation:
//!  - Concurrent single thread append with many concurrent readers.
//!
//! The underlying memory is memory mapped to a file. The accounts would be
//! stored across multiple files and the mappings of file and offset of a
//! particular account would be stored in a shared index. This will allow for
//! concurrent commits without blocking reads, which will sequentially write
//! to memory, ssd or disk, and should be as fast as the hardware allow for.
//! The only required in memory data structure with a write lock is the index,
//! which should be fast to update.
//!
//! AppendVec's only store accounts for single slots.  To bootstrap the
//! index from a persistent store of AppendVec's, the entries include
//! a "write_version".  A single global atomic `AccountsDB::write_version`
//! tracks the number of commits to the entire data store. So the latest
//! commit for each slot entry would be indexed.

use crate::{
    accounts_index::{AccountsIndex, Ancestors, SlotList, SlotSlice},
    append_vec::{AppendVec, StoredAccount, StoredMeta},
};
use blake3::traits::digest::Digest;
use lazy_static::lazy_static;
use log::*;
use rand::{thread_rng, Rng};
use rayon::{prelude::*, ThreadPool};
use serde::{Deserialize, Serialize};
use solana_measure::measure::Measure;
use solana_rayon_threadlimit::get_thread_count;
use solana_sdk::{
    account::Account,
    clock::{Epoch, Slot},
    genesis_config::ClusterType,
    hash::{Hash, Hasher},
    pubkey::Pubkey,
};
use std::convert::TryFrom;
use std::{
    collections::{HashMap, HashSet},
    convert::TryInto,
    io::{Error as IOError, Result as IOResult},
    iter::FromIterator,
    ops::RangeBounds,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard},
    time::Instant,
};
use tempfile::TempDir;

const PAGE_SIZE: u64 = 4 * 1024;
pub const DEFAULT_FILE_SIZE: u64 = PAGE_SIZE * 1024;
pub const DEFAULT_NUM_THREADS: u32 = 8;
pub const DEFAULT_NUM_DIRS: u32 = 4;

lazy_static! {
    // FROZEN_ACCOUNT_PANIC is used to signal local_cluster that an AccountsDB panic has occurred,
    // as |cargo test| cannot observe panics in other threads
    pub static ref FROZEN_ACCOUNT_PANIC: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
}

#[derive(Debug, Default)]
pub struct ErrorCounters {
    pub total: usize,
    pub account_in_use: usize,
    pub account_loaded_twice: usize,
    pub account_not_found: usize,
    pub blockhash_not_found: usize,
    pub blockhash_too_old: usize,
    pub call_chain_too_deep: usize,
    pub duplicate_signature: usize,
    pub instruction_error: usize,
    pub insufficient_funds: usize,
    pub invalid_account_for_fee: usize,
    pub invalid_account_index: usize,
    pub invalid_program_for_execution: usize,
    pub not_allowed_during_cluster_maintenance: usize,
}

#[derive(Default, Debug, PartialEq, Clone)]
pub struct AccountInfo {
    /// index identifying the append storage
    store_id: AppendVecId,

    /// offset into the storage
    offset: usize,

    /// lamports in the account used when squashing kept for optimization
    /// purposes to remove accounts with zero balance.
    lamports: u64,
}
/// An offset into the AccountsDB::storage vector
pub type AppendVecId = usize;
pub type SnapshotStorage = Vec<Arc<AccountStorageEntry>>;
pub type SnapshotStorages = Vec<SnapshotStorage>;

// Each slot has a set of storage entries.
pub(crate) type SlotStores = HashMap<usize, Arc<AccountStorageEntry>>;

trait Versioned {
    fn version(&self) -> u64;
}

impl Versioned for (u64, Hash) {
    fn version(&self) -> u64 {
        self.0
    }
}

impl Versioned for (u64, AccountInfo) {
    fn version(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Default, Debug)]
pub struct AccountStorage(pub HashMap<Slot, SlotStores>);

impl AccountStorage {
    fn scan_accounts(&self, account_info: &AccountInfo, slot: Slot) -> Option<(Account, Slot)> {
        self.0
            .get(&slot)
            .and_then(|storage_map| storage_map.get(&account_info.store_id))
            .and_then(|store| {
                Some(
                    store
                        .accounts
                        .get_account(account_info.offset)?
                        .0
                        .clone_account(),
                )
            })
            .map(|account| (account, slot))
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Deserialize, Serialize, AbiExample, AbiEnumVisitor)]
pub enum AccountStorageStatus {
    Available = 0,
    Full = 1,
    Candidate = 2,
}

impl Default for AccountStorageStatus {
    fn default() -> Self {
        Self::Available
    }
}

#[derive(Debug)]
pub enum BankHashVerificationError {
    MismatchedAccountHash,
    MismatchedBankHash,
    MissingBankHash,
    MismatchedTotalLamports(u64, u64),
}

/// Persistent storage structure holding the accounts
#[derive(Debug)]
pub struct AccountStorageEntry {
    pub(crate) id: AppendVecId,

    pub(crate) slot: Slot,

    /// storage holding the accounts
    pub(crate) accounts: AppendVec,

    /// Keeps track of the number of accounts stored in a specific AppendVec.
    ///  This is periodically checked to reuse the stores that do not have
    ///  any accounts in it
    /// status corresponding to the storage, lets us know that
    ///  the append_vec, once maxed out, then emptied, can be reclaimed
    count_and_status: RwLock<(usize, AccountStorageStatus)>,

    /// This is the total number of accounts stored ever since initialized to keep
    /// track of lifetime count of all store operations. And this differs from
    /// count_and_status in that this field won't be decremented.
    ///
    /// This is used as a rough estimate for slot shrinking. As such a relaxed
    /// use case, this value ARE NOT strictly synchronized with count_and_status!
    approx_store_count: AtomicUsize,
}

impl AccountStorageEntry {
    pub fn new(path: &Path, slot: Slot, id: usize, file_size: u64) -> Self {
        let tail = AppendVec::new_relative_path(slot, id);
        let path = Path::new(path).join(&tail);
        let accounts = AppendVec::new(&path, true, file_size as usize);

        Self {
            id,
            slot,
            accounts,
            count_and_status: RwLock::new((0, AccountStorageStatus::Available)),
            approx_store_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn new_empty_map(id: AppendVecId, accounts_current_len: usize) -> Self {
        Self {
            id,
            slot: 0,
            accounts: AppendVec::new_empty_map(accounts_current_len),
            count_and_status: RwLock::new((0, AccountStorageStatus::Available)),
            approx_store_count: AtomicUsize::new(0),
        }
    }

    pub fn set_status(&self, mut status: AccountStorageStatus) {
        let mut count_and_status = self.count_and_status.write().unwrap();

        let count = count_and_status.0;

        if status == AccountStorageStatus::Full && count == 0 {
            // this case arises when the append_vec is full (store_ptrs fails),
            //  but all accounts have already been removed from the storage
            //
            // the only time it's safe to call reset() on an append_vec is when
            //  every account has been removed
            //          **and**
            //  the append_vec has previously been completely full
            //
            self.accounts.reset();
            status = AccountStorageStatus::Available;
        }

        *count_and_status = (count, status);
    }

    pub fn status(&self) -> AccountStorageStatus {
        self.count_and_status.read().unwrap().1
    }

    pub fn count(&self) -> usize {
        self.count_and_status.read().unwrap().0
    }

    pub fn approx_stored_count(&self) -> usize {
        self.approx_store_count.load(Ordering::Relaxed)
    }

    pub fn has_accounts(&self) -> bool {
        self.count() > 0
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn append_vec_id(&self) -> AppendVecId {
        self.id
    }

    pub fn flush(&self) -> Result<(), IOError> {
        self.accounts.flush()
    }

    fn add_account(&self) {
        let mut count_and_status = self.count_and_status.write().unwrap();
        *count_and_status = (count_and_status.0 + 1, count_and_status.1);
        self.approx_store_count.fetch_add(1, Ordering::Relaxed);
    }

    fn try_available(&self) -> bool {
        let mut count_and_status = self.count_and_status.write().unwrap();
        let (count, status) = *count_and_status;

        if status == AccountStorageStatus::Available {
            *count_and_status = (count, AccountStorageStatus::Candidate);
            true
        } else {
            false
        }
    }

    fn remove_account(&self) -> usize {
        let mut count_and_status = self.count_and_status.write().unwrap();
        let (mut count, mut status) = *count_and_status;

        if count == 1 && status == AccountStorageStatus::Full {
            // this case arises when we remove the last account from the
            //  storage, but we've learned from previous write attempts that
            //  the storage is full
            //
            // the only time it's safe to call reset() on an append_vec is when
            //  every account has been removed
            //          **and**
            //  the append_vec has previously been completely full
            //
            // otherwise, the storage may be in flight with a store()
            //   call
            self.accounts.reset();
            status = AccountStorageStatus::Available;
        }

        // Some code path is removing accounts too many; this may result in an
        // unintended reveal of old state for unrelated accounts.
        assert!(
            count > 0,
            "double remove of account in slot: {}/store: {}!!",
            self.slot,
            self.id
        );

        count -= 1;
        *count_and_status = (count, status);
        count
    }

    pub fn set_file<P: AsRef<Path>>(&mut self, path: P) -> IOResult<()> {
        self.accounts.set_file(path)
    }

    pub fn get_relative_path(&self) -> Option<PathBuf> {
        AppendVec::get_relative_path(self.accounts.get_path())
    }

    pub fn get_path(&self) -> PathBuf {
        self.accounts.get_path()
    }
}

pub fn get_temp_accounts_paths(count: u32) -> IOResult<(Vec<TempDir>, Vec<PathBuf>)> {
    let temp_dirs: IOResult<Vec<TempDir>> = (0..count).map(|_| TempDir::new()).collect();
    let temp_dirs = temp_dirs?;
    let paths: Vec<PathBuf> = temp_dirs.iter().map(|t| t.path().to_path_buf()).collect();
    Ok((temp_dirs, paths))
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, AbiExample)]
pub struct BankHashStats {
    pub num_updated_accounts: u64,
    pub num_removed_accounts: u64,
    pub num_lamports_stored: u64,
    pub total_data_len: u64,
    pub num_executable_accounts: u64,
}

impl BankHashStats {
    pub fn update(&mut self, account: &Account) {
        if account.lamports == 0 {
            self.num_removed_accounts += 1;
        } else {
            self.num_updated_accounts += 1;
        }
        self.total_data_len = self.total_data_len.wrapping_add(account.data.len() as u64);
        if account.executable {
            self.num_executable_accounts += 1;
        }
        self.num_lamports_stored = self.num_lamports_stored.wrapping_add(account.lamports);
    }

    pub fn merge(&mut self, other: &BankHashStats) {
        self.num_updated_accounts += other.num_updated_accounts;
        self.num_removed_accounts += other.num_removed_accounts;
        self.total_data_len = self.total_data_len.wrapping_add(other.total_data_len);
        self.num_lamports_stored = self
            .num_lamports_stored
            .wrapping_add(other.num_lamports_stored);
        self.num_executable_accounts += other.num_executable_accounts;
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, AbiExample)]
pub struct BankHashInfo {
    pub hash: Hash,
    pub snapshot_hash: Hash,
    pub stats: BankHashStats,
}

#[derive(Debug)]
struct FrozenAccountInfo {
    pub hash: Hash,    // Hash generated by hash_frozen_account_data()
    pub lamports: u64, // Account balance cannot be lower than this amount
}

// This structure handles the load/store of the accounts
#[derive(Debug)]
pub struct AccountsDB {
    /// Keeps tracks of index into AppendVec on a per slot basis
    pub accounts_index: RwLock<AccountsIndex<AccountInfo>>,

    pub storage: RwLock<AccountStorage>,

    /// distribute the accounts across storage lists
    pub next_id: AtomicUsize,
    pub shrink_candidate_slots: Mutex<Vec<Slot>>,

    pub(crate) write_version: AtomicU64,

    /// Set of storage paths to pick from
    pub(crate) paths: Vec<PathBuf>,

    /// Directory of paths this accounts_db needs to hold/remove
    temp_paths: Option<Vec<TempDir>>,

    /// Starting file size of appendvecs
    file_size: u64,

    /// Accounts that will cause a panic! if data modified or lamports decrease
    frozen_accounts: HashMap<Pubkey, FrozenAccountInfo>,

    /// Thread pool used for par_iter
    pub thread_pool: ThreadPool,

    pub thread_pool_clean: ThreadPool,

    /// Number of append vecs to create to maximize parallelism when scanning
    /// the accounts
    min_num_stores: usize,

    pub bank_hashes: RwLock<HashMap<Slot, BankHashInfo>>,

    stats: AccountsStats,

    pub cluster_type: Option<ClusterType>,
}

#[derive(Debug, Default)]
struct AccountsStats {
    delta_hash_scan_time_total_us: AtomicU64,
    delta_hash_accumulate_time_total_us: AtomicU64,
    delta_hash_merge_time_total_us: AtomicU64,
    delta_hash_num: AtomicU64,
}

fn make_min_priority_thread_pool() -> ThreadPool {
    // Use lower thread count to reduce priority.
    let num_threads = std::cmp::max(2, num_cpus::get() / 4);
    rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("solana-accounts-cleanup-{}", i))
        .num_threads(num_threads)
        .build()
        .unwrap()
}

#[cfg(all(test, RUSTC_WITH_SPECIALIZATION))]
impl solana_sdk::abi_example::AbiExample for AccountsDB {
    fn example() -> Self {
        let accounts_db = AccountsDB::new_single();
        let key = Pubkey::default();
        let some_data_len = 5;
        let some_slot: Slot = 0;
        let account = Account::new(1, some_data_len, &key);
        accounts_db.store(some_slot, &[(&key, &account)]);
        accounts_db.add_root(0);

        accounts_db
    }
}

impl Default for AccountsDB {
    fn default() -> Self {
        let num_threads = get_thread_count();

        let mut bank_hashes = HashMap::new();
        bank_hashes.insert(0, BankHashInfo::default());
        AccountsDB {
            accounts_index: RwLock::new(AccountsIndex::default()),
            storage: RwLock::new(AccountStorage(HashMap::new())),
            next_id: AtomicUsize::new(0),
            shrink_candidate_slots: Mutex::new(Vec::new()),
            write_version: AtomicU64::new(0),
            paths: vec![],
            temp_paths: None,
            file_size: DEFAULT_FILE_SIZE,
            thread_pool: rayon::ThreadPoolBuilder::new()
                .num_threads(num_threads)
                .thread_name(|i| format!("solana-accounts-db-{}", i))
                .build()
                .unwrap(),
            thread_pool_clean: make_min_priority_thread_pool(),
            min_num_stores: num_threads,
            bank_hashes: RwLock::new(bank_hashes),
            frozen_accounts: HashMap::new(),
            stats: AccountsStats::default(),
            cluster_type: None,
        }
    }
}

impl AccountsDB {
    pub fn new(paths: Vec<PathBuf>, cluster_type: &ClusterType) -> Self {
        let new = if !paths.is_empty() {
            Self {
                paths,
                temp_paths: None,
                cluster_type: Some(*cluster_type),
                ..Self::default()
            }
        } else {
            // Create a temporary set of accounts directories, used primarily
            // for testing
            let (temp_dirs, paths) = get_temp_accounts_paths(DEFAULT_NUM_DIRS).unwrap();
            Self {
                paths,
                temp_paths: Some(temp_dirs),
                cluster_type: Some(*cluster_type),
                ..Self::default()
            }
        };
        {
            for path in new.paths.iter() {
                std::fs::create_dir_all(path).expect("Create directory failed.");
            }
        }
        new
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    #[cfg(test)]
    pub fn new_single() -> Self {
        AccountsDB {
            min_num_stores: 0,
            ..AccountsDB::new(Vec::new(), &ClusterType::Development)
        }
    }
    #[cfg(test)]
    pub fn new_sized(paths: Vec<PathBuf>, file_size: u64) -> Self {
        AccountsDB {
            file_size,
            ..AccountsDB::new(paths, &ClusterType::Development)
        }
    }

    fn new_storage_entry(&self, slot: Slot, path: &Path, size: u64) -> AccountStorageEntry {
        AccountStorageEntry::new(
            path,
            slot,
            self.next_id.fetch_add(1, Ordering::Relaxed),
            size,
        )
    }

    // Reclaim older states of rooted non-zero lamport accounts as a general
    // AccountsDB bloat mitigation and preprocess for better zero-lamport purging.
    fn clean_old_rooted_accounts(&self, purges_in_root: Vec<Pubkey>, max_clean_root: Option<Slot>) {
        // This number isn't carefully chosen; just guessed randomly such that
        // the hot loop will be the order of ~Xms.
        const INDEX_CLEAN_BULK_COUNT: usize = 4096;

        let mut clean_rooted = Measure::start("clean_old_root-ms");
        let reclaim_vecs =
            purges_in_root
                .par_chunks(INDEX_CLEAN_BULK_COUNT)
                .map(|pubkeys: &[Pubkey]| {
                    let mut reclaims = Vec::new();
                    let accounts_index = self.accounts_index.read().unwrap();
                    for pubkey in pubkeys {
                        accounts_index.clean_rooted_entries(&pubkey, &mut reclaims, max_clean_root);
                    }
                    reclaims
                });
        let reclaims: Vec<_> = reclaim_vecs.flatten().collect();
        clean_rooted.stop();
        inc_new_counter_info!("clean-old-root-par-clean-ms", clean_rooted.as_ms() as usize);

        let mut measure = Measure::start("clean_old_root_reclaims");
        self.handle_reclaims(&reclaims, None, false);
        measure.stop();
        debug!("{} {}", clean_rooted, measure);
        inc_new_counter_info!("clean-old-root-reclaim-ms", measure.as_ms() as usize);
    }

    fn do_reset_uncleaned_roots(
        &self,
        candidates: &mut MutexGuard<Vec<Slot>>,
        max_clean_root: Option<Slot>,
    ) {
        let previous_roots = self
            .accounts_index
            .write()
            .unwrap()
            .reset_uncleaned_roots(max_clean_root);
        candidates.extend(previous_roots);
    }

    #[cfg(test)]
    fn reset_uncleaned_roots(&self) {
        self.do_reset_uncleaned_roots(&mut self.shrink_candidate_slots.lock().unwrap(), None);
    }

    fn calc_delete_dependencies(
        purges: &HashMap<Pubkey, (SlotList<AccountInfo>, u64)>,
        store_counts: &mut HashMap<AppendVecId, (usize, HashSet<Pubkey>)>,
    ) {
        // Another pass to check if there are some filtered accounts which
        // do not match the criteria of deleting all appendvecs which contain them
        // then increment their storage count.
        let mut already_counted = HashSet::new();
        for (_pubkey, (account_infos, ref_count_from_storage)) in purges.iter() {
            let no_delete = if account_infos.len() as u64 != *ref_count_from_storage {
                true
            } else {
                let mut no_delete = false;
                for (_slot, account_info) in account_infos {
                    if store_counts.get(&account_info.store_id).unwrap().0 != 0 {
                        no_delete = true;
                        break;
                    }
                }
                no_delete
            };
            if no_delete {
                let mut pending_store_ids: HashSet<usize> = HashSet::new();
                for (_slot_id, account_info) in account_infos {
                    if !already_counted.contains(&account_info.store_id) {
                        pending_store_ids.insert(account_info.store_id);
                    }
                }
                while !pending_store_ids.is_empty() {
                    let id = pending_store_ids.iter().next().cloned().unwrap();
                    pending_store_ids.remove(&id);
                    if already_counted.contains(&id) {
                        continue;
                    }
                    store_counts.get_mut(&id).unwrap().0 += 1;
                    already_counted.insert(id);

                    let affected_pubkeys = &store_counts.get(&id).unwrap().1;
                    for key in affected_pubkeys {
                        for (_slot, account_info) in &purges.get(&key).unwrap().0 {
                            if !already_counted.contains(&account_info.store_id) {
                                pending_store_ids.insert(account_info.store_id);
                            }
                        }
                    }
                }
            }
        }
    }

    fn purge_keys_exact(
        &self,
        pubkey_to_slot_set: Vec<(Pubkey, HashSet<Slot>)>,
    ) -> (Vec<(u64, AccountInfo)>, Vec<Pubkey>) {
        let mut reclaims = Vec::new();
        let mut dead_keys = Vec::new();

        let accounts_index = self.accounts_index.read().unwrap();
        for (pubkey, slots_set) in pubkey_to_slot_set {
            let (new_reclaims, is_empty) = accounts_index.purge_exact(&pubkey, slots_set);
            if is_empty {
                dead_keys.push(pubkey);
            }
            reclaims.extend(new_reclaims);
        }

        (reclaims, dead_keys)
    }

    // Purge zero lamport accounts and older rooted account states as garbage
    // collection
    // Only remove those accounts where the entire rooted history of the account
    // can be purged because there are no live append vecs in the ancestors
    pub fn clean_accounts(&self, max_clean_root: Option<Slot>) {
        // hold a lock to prevent slot shrinking from running because it might modify some rooted
        // slot storages which can not happen as long as we're cleaning accounts because we're also
        // modifying the rooted slot storages!
        let mut candidates = self.shrink_candidate_slots.lock().unwrap();

        self.report_store_stats();

        let mut accounts_scan = Measure::start("accounts_scan");
        let accounts_index = self.accounts_index.read().unwrap();
        let pubkeys: Vec<Pubkey> = accounts_index.account_maps.keys().cloned().collect();
        // parallel scan the index.
        let (mut purges, purges_in_root) = pubkeys
            .par_chunks(4096)
            .map(|pubkeys: &[Pubkey]| {
                let mut purges_in_root = Vec::new();
                let mut purges = HashMap::new();
                for pubkey in pubkeys {
                    if let Some((list, index)) = accounts_index.get(pubkey, None, max_clean_root) {
                        let (slot, account_info) = &list[index];
                        if account_info.lamports == 0 {
                            purges.insert(*pubkey, accounts_index.would_purge(pubkey));
                        } else if accounts_index.uncleaned_roots.contains(slot) {
                            purges_in_root.push(*pubkey);
                        }
                    }
                }
                (purges, purges_in_root)
            })
            .reduce(
                || (HashMap::new(), Vec::new()),
                |mut m1, m2| {
                    // Collapse down the hashmaps/vecs into one.
                    m1.0.extend(m2.0);
                    m1.1.extend(m2.1);
                    m1
                },
            );

        drop(accounts_index);
        accounts_scan.stop();

        let mut clean_old_rooted = Measure::start("clean_old_roots");
        if !purges_in_root.is_empty() {
            self.clean_old_rooted_accounts(purges_in_root, max_clean_root);
        }
        self.do_reset_uncleaned_roots(&mut candidates, max_clean_root);
        clean_old_rooted.stop();

        let mut store_counts_time = Measure::start("store_counts");

        // Calculate store counts as if everything was purged
        // Then purge if we can
        let mut store_counts: HashMap<AppendVecId, (usize, HashSet<Pubkey>)> = HashMap::new();
        let storage = self.storage.read().unwrap();
        for (key, (account_infos, _ref_count)) in &purges {
            for (slot, account_info) in account_infos {
                let slot_storage = storage.0.get(&slot).unwrap();
                let store = slot_storage.get(&account_info.store_id).unwrap();
                if let Some(store_count) = store_counts.get_mut(&account_info.store_id) {
                    store_count.0 -= 1;
                    store_count.1.insert(*key);
                } else {
                    let mut key_set = HashSet::new();
                    key_set.insert(*key);
                    store_counts.insert(
                        account_info.store_id,
                        (store.count_and_status.read().unwrap().0 - 1, key_set),
                    );
                }
            }
        }
        store_counts_time.stop();
        drop(storage);

        let mut calc_deps_time = Measure::start("calc_deps");
        Self::calc_delete_dependencies(&purges, &mut store_counts);
        calc_deps_time.stop();

        // Only keep purges where the entire history of the account in the root set
        // can be purged. All AppendVecs for those updates are dead.
        let mut purge_filter = Measure::start("purge_filter");
        purges.retain(|_pubkey, (account_infos, _ref_count)| {
            for (_slot, account_info) in account_infos.iter() {
                if store_counts.get(&account_info.store_id).unwrap().0 != 0 {
                    return false;
                }
            }
            true
        });
        purge_filter.stop();

        let mut reclaims_time = Measure::start("reclaims");
        // Recalculate reclaims with new purge set
        let pubkey_to_slot_set: Vec<_> = purges
            .into_iter()
            .map(|(key, (slots_list, _ref_count))| {
                (
                    key,
                    HashSet::from_iter(slots_list.into_iter().map(|(slot, _)| slot)),
                )
            })
            .collect();

        let (reclaims, dead_keys) = self.purge_keys_exact(pubkey_to_slot_set);

        self.handle_dead_keys(dead_keys);

        self.handle_reclaims(&reclaims, None, false);

        reclaims_time.stop();
        datapoint_info!(
            "clean_accounts",
            ("accounts_scan", accounts_scan.as_us() as i64, i64),
            ("store_counts", store_counts_time.as_us() as i64, i64),
            ("purge_filter", purge_filter.as_us() as i64, i64),
            ("calc_deps", calc_deps_time.as_us() as i64, i64),
            ("reclaims", reclaims_time.as_us() as i64, i64),
        );
    }

    fn handle_dead_keys(&self, dead_keys: Vec<Pubkey>) {
        if !dead_keys.is_empty() {
            let mut accounts_index = self.accounts_index.write().unwrap();
            for key in &dead_keys {
                if let Some((_ref_count, list)) = accounts_index.account_maps.get(key) {
                    if list.read().unwrap().is_empty() {
                        accounts_index.account_maps.remove(key);
                    }
                }
            }
        }
    }

    // Removes the accounts in the input `reclaims` from the tracked "count" of
    // their corresponding  storage entries. Note this does not actually free
    // the memory from the storage entries until all the storage entries for
    // a given slot `S` are empty, at which point `process_dead_slots` will
    // remove all the storage entries for `S`.
    //
    /// # Arguments
    /// * `reclaims` - The accounts to remove from storage entries' "count"
    /// * `expected_single_dead_slot` - A correctness assertion. If this is equal to `Some(S)`,
    /// then the function will check that the only slot being cleaned up in `reclaims`
    /// is the slot == `S`. This is true for instance when `handle_reclaims` is called
    /// from store or slot shrinking, as those should only touch the slot they are
    /// currently storing to or shrinking.
    /// * `no_dead_slot` - A correctness assertion. If this is equal to
    /// `false`, the function will check that no slots are cleaned up/removed via
    /// `process_dead_slots`. For instance, on store, no slots should be cleaned up,
    /// but during the background clean accounts purges accounts from old rooted slots,
    /// so outdated slots may be removed.
    fn handle_reclaims(
        &self,
        reclaims: SlotSlice<AccountInfo>,
        expected_single_dead_slot: Option<Slot>,
        no_dead_slot: bool,
    ) {
        if !reclaims.is_empty() {
            let dead_slots = self.remove_dead_accounts(reclaims, expected_single_dead_slot);
            if no_dead_slot {
                assert!(dead_slots.is_empty());
            } else if let Some(expected_single_dead_slot) = expected_single_dead_slot {
                assert!(dead_slots.len() <= 1);
                if dead_slots.len() == 1 {
                    assert!(dead_slots.contains(&expected_single_dead_slot));
                }
            }
            if !dead_slots.is_empty() {
                self.process_dead_slots(&dead_slots);
            }
        }
    }

    // Must be kept private!, does sensitive cleanup that should only be called from
    // supported pipelines in AccountsDb
    fn process_dead_slots(&self, dead_slots: &HashSet<Slot>) {
        let mut clean_dead_slots = Measure::start("reclaims::purge_slots");
        self.clean_dead_slots(&dead_slots);
        clean_dead_slots.stop();

        let mut purge_slots = Measure::start("reclaims::purge_slots");
        self.purge_slots(&dead_slots);
        purge_slots.stop();

        debug!(
            "process_dead_slots({}): {} {}",
            dead_slots.len(),
            clean_dead_slots,
            purge_slots
        );
    }

    fn do_shrink_stale_slot(&self, slot: Slot) -> usize {
        self.do_shrink_slot(slot, false)
    }

    fn do_shrink_slot_forced(&self, slot: Slot) {
        self.do_shrink_slot(slot, true);
    }

    fn shrink_stale_slot(&self, candidates: &mut MutexGuard<Vec<Slot>>) -> usize {
        if let Some(slot) = self.do_next_shrink_slot(candidates) {
            self.do_shrink_stale_slot(slot)
        } else {
            0
        }
    }

    // Reads all accounts in given slot's AppendVecs and filter only to alive,
    // then create a minimum AppendVec filled with the alive.
    fn do_shrink_slot(&self, slot: Slot, forced: bool) -> usize {
        trace!("shrink_stale_slot: slot: {}", slot);

        let mut stored_accounts = vec![];
        let mut storage_read_elapsed = Measure::start("storage_read_elapsed");
        {
            let slot_storages = self.storage.read().unwrap().0.get(&slot).cloned();
            if let Some(stores) = slot_storages {
                let mut alive_count = 0;
                let mut stored_count = 0;
                for store in stores.values() {
                    alive_count += store.count();
                    stored_count += store.approx_stored_count();
                }
                if alive_count == stored_count && stores.values().len() == 1 {
                    trace!(
                        "shrink_stale_slot: not able to shrink at all{}: {} / {}",
                        alive_count,
                        stored_count,
                        if forced { " (forced)" } else { "" },
                    );
                    return 0;
                } else if (alive_count as f32 / stored_count as f32) >= 0.80 && !forced {
                    trace!(
                        "shrink_stale_slot: not enough space to shrink: {} / {}",
                        alive_count,
                        stored_count,
                    );
                    return 0;
                }
                for store in stores.values() {
                    let mut start = 0;
                    while let Some((account, next)) = store.accounts.get_account(start) {
                        stored_accounts.push((
                            account.meta.pubkey,
                            account.clone_account(),
                            *account.hash,
                            next - start,
                            (store.id, account.offset),
                            account.meta.write_version,
                        ));
                        start = next;
                    }
                }
            }
        }
        storage_read_elapsed.stop();

        let mut index_read_elapsed = Measure::start("index_read_elapsed");
        let alive_accounts: Vec<_> = {
            let accounts_index = self.accounts_index.read().unwrap();
            stored_accounts
                .iter()
                .filter(
                    |(
                        pubkey,
                        _account,
                        _account_hash,
                        _storage_size,
                        (store_id, offset),
                        _write_version,
                    )| {
                        if let Some((list, _)) = accounts_index.get(pubkey, None, None) {
                            list.iter()
                                .any(|(_slot, i)| i.store_id == *store_id && i.offset == *offset)
                        } else {
                            false
                        }
                    },
                )
                .collect()
        };
        index_read_elapsed.stop();

        let alive_total: u64 = alive_accounts
            .iter()
            .map(
                |(_pubkey, _account, _account_hash, account_size, _location, _write_verion)| {
                    *account_size as u64
                },
            )
            .sum();
        let aligned_total: u64 = (alive_total + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1);

        debug!(
            "shrinking: slot: {}, stored_accounts: {} => alive_accounts: {} ({} bytes; aligned to: {})",
            slot,
            stored_accounts.len(),
            alive_accounts.len(),
            alive_total,
            aligned_total
        );

        let mut rewrite_elapsed = Measure::start("rewrite_elapsed");
        let mut dead_storages = vec![];
        let mut find_alive_elapsed = 0;
        let mut create_and_insert_store_elapsed = 0;
        let mut store_accounts_elapsed = 0;
        let mut update_index_elapsed = 0;
        let mut handle_reclaims_elapsed = 0;
        let mut write_storage_elapsed = 0;
        if aligned_total > 0 {
            let mut start = Measure::start("find_alive_elapsed");
            let mut accounts = Vec::with_capacity(alive_accounts.len());
            let mut hashes = Vec::with_capacity(alive_accounts.len());
            let mut write_versions = Vec::with_capacity(alive_accounts.len());

            for (pubkey, account, account_hash, _size, _location, write_version) in &alive_accounts
            {
                accounts.push((pubkey, account));
                hashes.push(*account_hash);
                write_versions.push(*write_version);
            }
            start.stop();
            find_alive_elapsed = start.as_us();

            let mut start = Measure::start("create_and_insert_store_elapsed");
            let shrunken_store = self.create_and_insert_store(slot, aligned_total);
            start.stop();
            create_and_insert_store_elapsed = start.as_us();

            // here, we're writing back alive_accounts. That should be an atomic operation
            // without use of rather wide locks in this whole function, because we're
            // mutating rooted slots; There should be no writers to them.
            let mut start = Measure::start("store_accounts_elapsed");
            let infos = self.store_accounts_to(
                slot,
                &accounts,
                &hashes,
                |_| shrunken_store.clone(),
                write_versions.into_iter(),
            );
            start.stop();
            store_accounts_elapsed = start.as_us();

            let mut start = Measure::start("update_index_elapsed");
            let reclaims = self.update_index(slot, infos, &accounts);
            start.stop();
            update_index_elapsed = start.as_us();

            let mut start = Measure::start("update_index_elapsed");
            self.handle_reclaims(&reclaims, Some(slot), true);
            start.stop();
            handle_reclaims_elapsed = start.as_us();

            let mut start = Measure::start("write_storage_elapsed");
            let mut storage = self.storage.write().unwrap();
            if let Some(slot_storage) = storage.0.get_mut(&slot) {
                slot_storage.retain(|_key, store| {
                    if store.count() == 0 {
                        dead_storages.push(store.clone());
                    }
                    store.count() > 0
                });
            }
            start.stop();
            write_storage_elapsed = start.as_us();
        }
        rewrite_elapsed.stop();

        let mut drop_storage_entries_elapsed = Measure::start("drop_storage_entries_elapsed");
        drop(dead_storages);
        drop_storage_entries_elapsed.stop();

        datapoint_info!(
            "do_shrink_slot_time",
            ("storage_read_elapsed", storage_read_elapsed.as_us(), i64),
            ("index_read_elapsed", index_read_elapsed.as_us(), i64),
            ("find_alive_elapsed", find_alive_elapsed, i64),
            (
                "create_and_insert_store_elapsed",
                create_and_insert_store_elapsed,
                i64
            ),
            ("store_accounts_elapsed", store_accounts_elapsed, i64),
            ("update_index_elapsed", update_index_elapsed, i64),
            ("handle_reclaims_elapsed", handle_reclaims_elapsed, i64),
            ("write_storage_elapsed", write_storage_elapsed, i64),
            ("rewrite_elapsed", rewrite_elapsed.as_us(), i64),
            (
                "drop_storage_entries_elapsed",
                drop_storage_entries_elapsed.as_us(),
                i64
            ),
        );
        alive_accounts.len()
    }

    // Infinitely returns rooted roots in cyclic order
    fn do_next_shrink_slot(&self, candidates: &mut MutexGuard<Vec<Slot>>) -> Option<Slot> {
        // At this point, a lock (= candidates) is ensured to be held to keep
        // do_reset_uncleaned_roots() (in clean_accounts()) from updating candidates.
        // Also, candidates in the lock may be swapped here if it's empty.
        let next = candidates.pop();

        if next.is_some() {
            next
        } else {
            let mut new_all_slots = self.all_root_slots_in_index();
            let next = new_all_slots.pop();
            // refresh candidates for later calls!
            **candidates = new_all_slots;

            next
        }
    }

    #[cfg(test)]
    fn next_shrink_slot(&self) -> Option<Slot> {
        let mut candidates = self.shrink_candidate_slots.lock().unwrap();
        self.do_next_shrink_slot(&mut candidates)
    }

    fn all_root_slots_in_index(&self) -> Vec<Slot> {
        let index = self.accounts_index.read().unwrap();
        index.roots.iter().cloned().collect()
    }

    fn all_slots_in_storage(&self) -> Vec<Slot> {
        let storage = self.storage.read().unwrap();
        storage.0.keys().cloned().collect()
    }

    pub fn process_stale_slot(&self) -> usize {
        let mut measure = Measure::start("stale_slot_shrink-ms");
        let candidates = self.shrink_candidate_slots.try_lock();
        if candidates.is_err() {
            // skip and return immediately if locked by clean_accounts()
            // the calling background thread will just retry later.
            return 0;
        }
        // hold this lock as long as this shrinking process is running to avoid conflicts
        // with clean_accounts().
        let mut candidates = candidates.unwrap();

        let count = self.shrink_stale_slot(&mut candidates);
        measure.stop();
        inc_new_counter_info!("stale_slot_shrink-ms", measure.as_ms() as usize);

        count
    }

    #[cfg(test)]
    fn shrink_all_stale_slots(&self) {
        for slot in self.all_slots_in_storage() {
            self.do_shrink_stale_slot(slot);
        }
    }

    pub fn shrink_all_slots(&self) {
        for slot in self.all_slots_in_storage() {
            self.do_shrink_slot_forced(slot);
        }
    }

    pub fn scan_accounts<F, A>(&self, ancestors: &Ancestors, scan_func: F) -> A
    where
        F: Fn(&mut A, Option<(&Pubkey, Account, Slot)>),
        A: Default,
    {
        let mut collector = A::default();
        let accounts_index = self.accounts_index.read().unwrap();
        let storage = self.storage.read().unwrap();
        accounts_index.scan_accounts(ancestors, |pubkey, (account_info, slot)| {
            scan_func(
                &mut collector,
                storage
                    .scan_accounts(account_info, slot)
                    .map(|(account, slot)| (pubkey, account, slot)),
            )
        });
        collector
    }

    pub fn range_scan_accounts<F, A, R>(&self, ancestors: &Ancestors, range: R, scan_func: F) -> A
    where
        F: Fn(&mut A, Option<(&Pubkey, Account, Slot)>),
        A: Default,
        R: RangeBounds<Pubkey>,
    {
        let mut collector = A::default();
        let accounts_index = self.accounts_index.read().unwrap();
        let storage = self.storage.read().unwrap();
        accounts_index.range_scan_accounts(ancestors, range, |pubkey, (account_info, slot)| {
            scan_func(
                &mut collector,
                storage
                    .scan_accounts(account_info, slot)
                    .map(|(account, slot)| (pubkey, account, slot)),
            )
        });
        collector
    }

    /// Scan a specific slot through all the account storage in parallel with sequential read
    // PERF: Sequentially read each storage entry in parallel
    pub fn scan_account_storage<F, B>(&self, slot: Slot, scan_func: F) -> Vec<B>
    where
        F: Fn(&StoredAccount, AppendVecId, &mut B) + Send + Sync,
        B: Send + Default,
    {
        self.scan_account_storage_inner(slot, scan_func, &self.storage.read().unwrap())
    }

    // The input storage must come from self.storage.read().unwrap()
    fn scan_account_storage_inner<F, B>(
        &self,
        slot: Slot,
        scan_func: F,
        storage: &RwLockReadGuard<AccountStorage>,
    ) -> Vec<B>
    where
        F: Fn(&StoredAccount, AppendVecId, &mut B) + Send + Sync,
        B: Send + Default,
    {
        let storage_maps: Vec<Arc<AccountStorageEntry>> = storage
            .0
            .get(&slot)
            .unwrap_or(&HashMap::new())
            .values()
            .cloned()
            .collect();
        self.thread_pool.install(|| {
            storage_maps
                .into_par_iter()
                .map(|storage| {
                    let accounts = storage.accounts.accounts(0);
                    let mut retval = B::default();
                    accounts.iter().for_each(|stored_account| {
                        scan_func(stored_account, storage.id, &mut retval)
                    });
                    retval
                })
                .collect()
        })
    }

    pub fn set_hash(&self, slot: Slot, parent_slot: Slot) {
        let mut bank_hashes = self.bank_hashes.write().unwrap();
        if bank_hashes.get(&slot).is_some() {
            error!(
                "set_hash: already exists; multiple forks with shared slot {} as child (parent: {})!?",
                slot, parent_slot,
            );
            return;
        }

        let new_hash_info = BankHashInfo {
            hash: Hash::default(),
            snapshot_hash: Hash::default(),
            stats: BankHashStats::default(),
        };
        bank_hashes.insert(slot, new_hash_info);
    }

    pub fn load(
        storage: &AccountStorage,
        ancestors: &Ancestors,
        accounts_index: &AccountsIndex<AccountInfo>,
        pubkey: &Pubkey,
    ) -> Option<(Account, Slot)> {
        let (lock, index) = accounts_index.get(pubkey, Some(ancestors), None)?;
        let slot = lock[index].0;
        //TODO: thread this as a ref
        if let Some(slot_storage) = storage.0.get(&slot) {
            let info = &lock[index].1;
            slot_storage
                .get(&info.store_id)
                .and_then(|store| Some(store.accounts.get_account(info.offset)?.0.clone_account()))
                .map(|account| (account, slot))
        } else {
            None
        }
    }

    #[cfg(test)]
    fn load_account_hash(&self, ancestors: &Ancestors, pubkey: &Pubkey) -> Hash {
        let accounts_index = self.accounts_index.read().unwrap();
        let (lock, index) = accounts_index.get(pubkey, Some(ancestors), None).unwrap();
        let slot = lock[index].0;
        let storage = self.storage.read().unwrap();
        let slot_storage = storage.0.get(&slot).unwrap();
        let info = &lock[index].1;
        let entry = slot_storage.get(&info.store_id).unwrap();
        let account = entry.accounts.get_account(info.offset);
        *account.as_ref().unwrap().0.hash
    }

    pub fn load_slow(&self, ancestors: &Ancestors, pubkey: &Pubkey) -> Option<(Account, Slot)> {
        let accounts_index = self.accounts_index.read().unwrap();
        let storage = self.storage.read().unwrap();
        Self::load(&storage, ancestors, &accounts_index, pubkey)
    }

    fn find_storage_candidate(&self, slot: Slot) -> Arc<AccountStorageEntry> {
        let mut create_extra = false;
        let stores = self.storage.read().unwrap();

        if let Some(slot_stores) = stores.0.get(&slot) {
            if !slot_stores.is_empty() {
                if slot_stores.len() <= self.min_num_stores {
                    let mut total_accounts = 0;
                    for store in slot_stores.values() {
                        total_accounts += store.count_and_status.read().unwrap().0;
                    }

                    // Create more stores so that when scanning the storage all CPUs have work
                    if (total_accounts / 16) >= slot_stores.len() {
                        create_extra = true;
                    }
                }

                // pick an available store at random by iterating from a random point
                let to_skip = thread_rng().gen_range(0, slot_stores.len());

                for (i, store) in slot_stores.values().cycle().skip(to_skip).enumerate() {
                    if store.try_available() {
                        let ret = store.clone();
                        drop(stores);
                        if create_extra {
                            self.create_and_insert_store(slot, self.file_size);
                        }
                        return ret;
                    }
                    // looked at every store, bail...
                    if i == slot_stores.len() {
                        break;
                    }
                }
            }
        }

        drop(stores);

        let store = self.create_and_insert_store(slot, self.file_size);
        store.try_available();
        store
    }

    fn create_and_insert_store(&self, slot: Slot, size: u64) -> Arc<AccountStorageEntry> {
        let path_index = thread_rng().gen_range(0, self.paths.len());
        let store =
            Arc::new(self.new_storage_entry(slot, &Path::new(&self.paths[path_index]), size));
        let store_for_index = store.clone();

        let mut stores = self.storage.write().unwrap();
        let slot_storage = stores.0.entry(slot).or_insert_with(HashMap::new);
        slot_storage.insert(store.id, store_for_index);
        store
    }

    pub fn purge_slot(&self, slot: Slot) {
        let mut slots = HashSet::new();
        slots.insert(slot);
        self.purge_slots(&slots);
    }

    fn purge_slots(&self, slots: &HashSet<Slot>) {
        //add_root should be called first
        let accounts_index = self.accounts_index.read().unwrap();
        let non_roots: Vec<_> = slots
            .iter()
            .filter(|slot| !accounts_index.is_root(**slot))
            .collect();
        drop(accounts_index);
        let mut storage_lock_elapsed = Measure::start("storage_lock_elapsed");
        let mut storage = self.storage.write().unwrap();
        storage_lock_elapsed.stop();

        let mut all_removed_slot_storages = vec![];
        let mut total_removed_storage_entries = 0;
        let mut total_removed_bytes = 0;

        let mut remove_storages_elapsed = Measure::start("remove_storages_elapsed");
        for slot in non_roots {
            if let Some(slot_removed_storages) = storage.0.remove(&slot) {
                total_removed_storage_entries += slot_removed_storages.len();
                total_removed_bytes += slot_removed_storages
                    .values()
                    .map(|i| i.accounts.capacity())
                    .sum::<u64>();
                all_removed_slot_storages.push(slot_removed_storages);
            }
        }
        remove_storages_elapsed.stop();
        drop(storage);

        let num_slots_removed = all_removed_slot_storages.len();

        let mut drop_storage_entries_elapsed = Measure::start("drop_storage_entries_elapsed");
        // Backing mmaps for removed storages entries explicitly dropped here outside
        // of any locks
        drop(all_removed_slot_storages);
        drop_storage_entries_elapsed.stop();

        datapoint_info!(
            "purge_slots_time",
            ("storage_lock_elapsed", storage_lock_elapsed.as_us(), i64),
            (
                "remove_storages_elapsed",
                remove_storages_elapsed.as_us(),
                i64
            ),
            (
                "drop_storage_entries_elapsed",
                drop_storage_entries_elapsed.as_us(),
                i64
            ),
            ("num_slots_removed", num_slots_removed, i64),
            (
                "total_removed_storage_entries",
                total_removed_storage_entries,
                i64
            ),
            ("total_removed_bytes", total_removed_bytes, i64),
        );
    }

    pub fn remove_unrooted_slot(&self, remove_slot: Slot) {
        if self.accounts_index.read().unwrap().is_root(remove_slot) {
            panic!("Trying to remove accounts for rooted slot {}", remove_slot);
        }

        let pubkey_sets: Vec<HashSet<Pubkey>> = self.scan_account_storage(
            remove_slot,
            |stored_account: &StoredAccount, _, accum: &mut HashSet<Pubkey>| {
                accum.insert(stored_account.meta.pubkey);
            },
        );

        // Purge this slot from the accounts index
        let mut reclaims = vec![];
        {
            let pubkeys = pubkey_sets.iter().flatten();
            let accounts_index = self.accounts_index.read().unwrap();

            for pubkey in pubkeys {
                accounts_index.clean_unrooted_entries_by_slot(remove_slot, pubkey, &mut reclaims);
            }
        }

        // 1) Remove old bank hash from self.bank_hashes
        // 2) Purge this slot's storage entries from self.storage
        self.handle_reclaims(&reclaims, Some(remove_slot), false);
        assert!(self.storage.read().unwrap().0.get(&remove_slot).is_none());
    }

    fn include_owner(cluster_type: &ClusterType, slot: Slot) -> bool {
        // When devnet was moved to stable release channel, it was done without
        // hashing account.owner. That's because devnet's slot was lower than
        // 5_800_000 and the release channel's gating lacked ClusterType at the time...
        match cluster_type {
            ClusterType::Devnet => slot >= 5_800_000,
            _ => true,
        }
    }

    pub fn hash_stored_account(
        slot: Slot,
        account: &StoredAccount,
        cluster_type: &ClusterType,
    ) -> Hash {
        let include_owner = Self::include_owner(cluster_type, slot);

        if slot > Self::get_blake3_slot(cluster_type) {
            Self::blake3_hash_account_data(
                slot,
                account.account_meta.lamports,
                &account.account_meta.owner,
                account.account_meta.executable,
                account.account_meta.rent_epoch,
                account.data,
                &account.meta.pubkey,
                include_owner,
            )
        } else {
            Self::hash_account_data(
                slot,
                account.account_meta.lamports,
                &account.account_meta.owner,
                account.account_meta.executable,
                account.account_meta.rent_epoch,
                account.data,
                &account.meta.pubkey,
                include_owner,
            )
        }
    }

    pub fn hash_account(
        slot: Slot,
        account: &Account,
        pubkey: &Pubkey,
        cluster_type: &ClusterType,
    ) -> Hash {
        let include_owner = Self::include_owner(cluster_type, slot);

        if slot > Self::get_blake3_slot(cluster_type) {
            Self::blake3_hash_account_data(
                slot,
                account.lamports,
                &account.owner,
                account.executable,
                account.rent_epoch,
                &account.data,
                pubkey,
                include_owner,
            )
        } else {
            Self::hash_account_data(
                slot,
                account.lamports,
                &account.owner,
                account.executable,
                account.rent_epoch,
                &account.data,
                pubkey,
                include_owner,
            )
        }
    }

    fn hash_frozen_account_data(account: &Account) -> Hash {
        let mut hasher = Hasher::default();

        hasher.hash(&account.data);
        hasher.hash(&account.owner.as_ref());

        if account.executable {
            hasher.hash(&[1u8; 1]);
        } else {
            hasher.hash(&[0u8; 1]);
        }

        hasher.result()
    }

    pub fn hash_account_data(
        slot: Slot,
        lamports: u64,
        owner: &Pubkey,
        executable: bool,
        rent_epoch: Epoch,
        data: &[u8],
        pubkey: &Pubkey,
        include_owner: bool,
    ) -> Hash {
        if lamports == 0 {
            return Hash::default();
        }

        let mut hasher = Hasher::default();

        hasher.hash(&lamports.to_le_bytes());

        hasher.hash(&slot.to_le_bytes());

        hasher.hash(&rent_epoch.to_le_bytes());

        hasher.hash(&data);

        if executable {
            hasher.hash(&[1u8; 1]);
        } else {
            hasher.hash(&[0u8; 1]);
        }

        if include_owner {
            hasher.hash(&owner.as_ref());
        }
        hasher.hash(&pubkey.as_ref());

        hasher.result()
    }

    pub fn blake3_hash_account_data(
        slot: Slot,
        lamports: u64,
        owner: &Pubkey,
        executable: bool,
        rent_epoch: Epoch,
        data: &[u8],
        pubkey: &Pubkey,
        include_owner: bool,
    ) -> Hash {
        if lamports == 0 {
            return Hash::default();
        }

        let mut hasher = blake3::Hasher::new();

        hasher.update(&lamports.to_le_bytes());

        hasher.update(&slot.to_le_bytes());

        hasher.update(&rent_epoch.to_le_bytes());

        hasher.update(&data);

        if executable {
            hasher.update(&[1u8; 1]);
        } else {
            hasher.update(&[0u8; 1]);
        }

        if include_owner {
            hasher.update(&owner.as_ref());
        }
        hasher.update(&pubkey.as_ref());

        Hash(<[u8; solana_sdk::hash::HASH_BYTES]>::try_from(hasher.finalize().as_slice()).unwrap())
    }

    fn get_blake3_slot(cluster_type: &ClusterType) -> Slot {
        match cluster_type {
            ClusterType::Development => 0,
            // Epoch 400
            ClusterType::Devnet => 3_276_800,
            // Epoch 78
            ClusterType::MainnetBeta => 33_696_000,
            // Epoch 95
            ClusterType::Testnet => 35_516_256,
        }
    }

    fn bulk_assign_write_version(&self, count: usize) -> u64 {
        self.write_version
            .fetch_add(count as u64, Ordering::Relaxed)
    }

    fn store_accounts(
        &self,
        slot: Slot,
        accounts: &[(&Pubkey, &Account)],
        hashes: &[Hash],
    ) -> Vec<AccountInfo> {
        let mut current_version = self.bulk_assign_write_version(accounts.len());
        let write_version_producer = std::iter::from_fn(move || {
            let ret = current_version;
            current_version += 1;
            Some(ret)
        });

        let storage_finder = |slot| self.find_storage_candidate(slot);
        self.store_accounts_to(
            slot,
            accounts,
            hashes,
            storage_finder,
            write_version_producer,
        )
    }

    fn store_accounts_to<F: FnMut(Slot) -> Arc<AccountStorageEntry>, P: Iterator<Item = u64>>(
        &self,
        slot: Slot,
        accounts: &[(&Pubkey, &Account)],
        hashes: &[Hash],
        mut storage_finder: F,
        mut write_version_producer: P,
    ) -> Vec<AccountInfo> {
        let default_account = Account::default();
        let with_meta: Vec<(StoredMeta, &Account)> = accounts
            .iter()
            .map(|(pubkey, account)| {
                let account = if account.lamports == 0 {
                    &default_account
                } else {
                    *account
                };
                let data_len = account.data.len() as u64;

                let meta = StoredMeta {
                    write_version: write_version_producer.next().unwrap(),
                    pubkey: **pubkey,
                    data_len,
                };
                (meta, account)
            })
            .collect();
        let mut infos: Vec<AccountInfo> = Vec::with_capacity(with_meta.len());
        while infos.len() < with_meta.len() {
            let storage = storage_finder(slot);
            let rvs = storage
                .accounts
                .append_accounts(&with_meta[infos.len()..], &hashes[infos.len()..]);
            if rvs.is_empty() {
                storage.set_status(AccountStorageStatus::Full);

                // See if an account overflows the default append vec size.
                let data_len = (with_meta[infos.len()].1.data.len() + 4096) as u64;
                if data_len > self.file_size {
                    self.create_and_insert_store(slot, data_len * 2);
                }
                continue;
            }
            for (offset, (_, account)) in rvs.iter().zip(&with_meta[infos.len()..]) {
                storage.add_account();
                infos.push(AccountInfo {
                    store_id: storage.id,
                    offset: *offset,
                    lamports: account.lamports,
                });
            }
            // restore the state to available
            storage.set_status(AccountStorageStatus::Available);
        }
        infos
    }

    fn report_store_stats(&self) {
        let mut total_count = 0;
        let mut min = std::usize::MAX;
        let mut min_slot = 0;
        let mut max = 0;
        let mut max_slot = 0;
        let mut newest_slot = 0;
        let mut oldest_slot = std::u64::MAX;
        let stores = self.storage.read().unwrap();
        for (slot, slot_stores) in &stores.0 {
            total_count += slot_stores.len();
            if slot_stores.len() < min {
                min = slot_stores.len();
                min_slot = *slot;
            }

            if slot_stores.len() > max {
                max = slot_stores.len();
                max_slot = *slot;
            }
            if *slot > newest_slot {
                newest_slot = *slot;
            }

            if *slot < oldest_slot {
                oldest_slot = *slot;
            }
        }
        drop(stores);
        info!("total_stores: {}, newest_slot: {}, oldest_slot: {}, max_slot: {} (num={}), min_slot: {} (num={})",
              total_count, newest_slot, oldest_slot, max_slot, max, min_slot, min);
        datapoint_info!("accounts_db-stores", ("total_count", total_count, i64));
        datapoint_info!(
            "accounts_db-perf-stats",
            (
                "delta_hash_num",
                self.stats.delta_hash_num.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "delta_hash_scan_us",
                self.stats
                    .delta_hash_scan_time_total_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "delta_hash_merge_us",
                self.stats
                    .delta_hash_merge_time_total_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "delta_hash_accumulate_us",
                self.stats
                    .delta_hash_accumulate_time_total_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }

    pub fn compute_merkle_root(hashes: Vec<(Pubkey, Hash, u64)>, fanout: usize) -> Hash {
        let hashes: Vec<_> = hashes
            .into_iter()
            .map(|(_pubkey, hash, _lamports)| hash)
            .collect();
        let mut hashes: Vec<_> = hashes.chunks(fanout).map(|x| x.to_vec()).collect();
        while hashes.len() > 1 {
            let mut time = Measure::start("time");
            let new_hashes: Vec<Hash> = hashes
                .par_iter()
                .map(|h| {
                    let mut hasher = Hasher::default();
                    for v in h.iter() {
                        hasher.hash(v.as_ref());
                    }
                    hasher.result()
                })
                .collect();
            time.stop();
            debug!("hashing {} {}", hashes.len(), time);
            hashes = new_hashes.chunks(fanout).map(|x| x.to_vec()).collect();
        }
        let mut hasher = Hasher::default();
        hashes.into_iter().flatten().for_each(|hash| {
            hasher.hash(hash.as_ref());
        });
        hasher.result()
    }

    fn accumulate_account_hashes(hashes: Vec<(Pubkey, Hash, u64)>) -> Hash {
        let (hash, ..) = Self::do_accumulate_account_hashes_and_capitalization(hashes, false);
        hash
    }

    fn accumulate_account_hashes_and_capitalization(
        hashes: Vec<(Pubkey, Hash, u64)>,
    ) -> (Hash, u64) {
        let (hash, cap) = Self::do_accumulate_account_hashes_and_capitalization(hashes, true);
        (hash, cap.unwrap())
    }

    fn do_accumulate_account_hashes_and_capitalization(
        mut hashes: Vec<(Pubkey, Hash, u64)>,
        calculate_cap: bool,
    ) -> (Hash, Option<u64>) {
        let mut sort_time = Measure::start("sort");
        hashes.par_sort_by(|a, b| a.0.cmp(&b.0));
        sort_time.stop();

        let mut sum_time = Measure::start("cap");
        let cap = if calculate_cap {
            Some(Self::checked_sum_for_capitalization(
                hashes.iter().map(|(_, _, lamports)| *lamports),
            ))
        } else {
            None
        };
        sum_time.stop();

        let mut hash_time = Measure::start("hash");
        let fanout = 16;
        let res = Self::compute_merkle_root(hashes, fanout);
        hash_time.stop();

        debug!("{} {} {}", sort_time, hash_time, sum_time);

        (res, cap)
    }

    pub fn checked_sum_for_capitalization<T: Iterator<Item = u64>>(balances: T) -> u64 {
        balances
            .map(|b| b as u128)
            .sum::<u128>()
            .try_into()
            .expect("overflow is detected while summing capitalization")
    }

    pub fn account_balance_for_capitalization(
        lamports: u64,
        owner: &Pubkey,
        executable: bool,
    ) -> u64 {
        let is_specially_retained = (solana_sdk::native_loader::check_id(owner) && executable)
            || solana_sdk::sysvar::check_id(owner);

        if is_specially_retained {
            // specially retained accounts always have an initial 1 lamport
            // balance, but could be modified by transfers which increase
            // the balance but don't affect the capitalization.
            lamports - 1
        } else {
            lamports
        }
    }

    fn calculate_accounts_hash(
        &self,
        ancestors: &Ancestors,
        check_hash: bool,
    ) -> Result<(Hash, u64), BankHashVerificationError> {
        use BankHashVerificationError::*;
        let mut scan = Measure::start("scan");
        let accounts_index = self.accounts_index.read().unwrap();
        let storage = self.storage.read().unwrap();
        let keys: Vec<_> = accounts_index.account_maps.keys().collect();
        let mismatch_found = AtomicU64::new(0);
        let hashes: Vec<_> = keys
            .par_iter()
            .filter_map(|pubkey| {
                if let Some((list, index)) = accounts_index.get(pubkey, Some(ancestors), None) {
                    let (slot, account_info) = &list[index];
                    if account_info.lamports != 0 {
                        storage
                            .0
                            .get(&slot)
                            .and_then(|storage_map| storage_map.get(&account_info.store_id))
                            .and_then(|store| {
                                let account = store.accounts.get_account(account_info.offset)?.0;
                                let balance = Self::account_balance_for_capitalization(
                                    account_info.lamports,
                                    &account.account_meta.owner,
                                    account.account_meta.executable,
                                );

                                if check_hash {
                                    let hash = Self::hash_stored_account(
                                        *slot,
                                        &account,
                                        &self
                                            .cluster_type
                                            .expect("Cluster type must be set at initialization"),
                                    );
                                    if hash != *account.hash {
                                        mismatch_found.fetch_add(1, Ordering::Relaxed);
                                        return None;
                                    }
                                }

                                Some((**pubkey, *account.hash, balance))
                            })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        if mismatch_found.load(Ordering::Relaxed) > 0 {
            warn!(
                "{} mismatched account hash(es) found",
                mismatch_found.load(Ordering::Relaxed)
            );
            return Err(MismatchedAccountHash);
        }

        scan.stop();
        let hash_total = hashes.len();

        let mut accumulate = Measure::start("accumulate");
        let (accumulated_hash, total_lamports) =
            Self::accumulate_account_hashes_and_capitalization(hashes);
        accumulate.stop();
        datapoint_info!(
            "update_accounts_hash",
            ("accounts_scan", scan.as_us(), i64),
            ("hash_accumulate", accumulate.as_us(), i64),
            ("hash_total", hash_total, i64),
        );
        Ok((accumulated_hash, total_lamports))
    }

    pub fn get_accounts_hash(&self, slot: Slot) -> Hash {
        let bank_hashes = self.bank_hashes.read().unwrap();
        let bank_hash_info = bank_hashes.get(&slot).unwrap();
        bank_hash_info.snapshot_hash
    }

    pub fn update_accounts_hash(&self, slot: Slot, ancestors: &Ancestors) -> (Hash, u64) {
        let (hash, total_lamports) = self.calculate_accounts_hash(ancestors, false).unwrap();
        let mut bank_hashes = self.bank_hashes.write().unwrap();
        let mut bank_hash_info = bank_hashes.get_mut(&slot).unwrap();
        bank_hash_info.snapshot_hash = hash;
        (hash, total_lamports)
    }

    pub fn verify_bank_hash_and_lamports(
        &self,
        slot: Slot,
        ancestors: &Ancestors,
        total_lamports: u64,
    ) -> Result<(), BankHashVerificationError> {
        use BankHashVerificationError::*;

        let (calculated_hash, calculated_lamports) =
            self.calculate_accounts_hash(ancestors, true)?;

        if calculated_lamports != total_lamports {
            warn!(
                "Mismatched total lamports: {} calculated: {}",
                total_lamports, calculated_lamports
            );
            return Err(MismatchedTotalLamports(calculated_lamports, total_lamports));
        }

        let bank_hashes = self.bank_hashes.read().unwrap();
        if let Some(found_hash_info) = bank_hashes.get(&slot) {
            if calculated_hash == found_hash_info.snapshot_hash {
                Ok(())
            } else {
                warn!(
                    "mismatched bank hash for slot {}: {} (calculated) != {} (expected)",
                    slot, calculated_hash, found_hash_info.snapshot_hash
                );
                Err(MismatchedBankHash)
            }
        } else {
            Err(MissingBankHash)
        }
    }

    pub fn get_accounts_delta_hash(&self, slot: Slot) -> Hash {
        let mut scan = Measure::start("scan");
        let mut accumulator: Vec<HashMap<Pubkey, (u64, Hash)>> = self.scan_account_storage(
            slot,
            |stored_account: &StoredAccount,
             _store_id: AppendVecId,
             accum: &mut HashMap<Pubkey, (u64, Hash)>| {
                accum.insert(
                    stored_account.meta.pubkey,
                    (stored_account.meta.write_version, *stored_account.hash),
                );
            },
        );
        scan.stop();
        let mut merge = Measure::start("merge");
        let mut account_maps = HashMap::new();
        while let Some(maps) = accumulator.pop() {
            AccountsDB::merge(&mut account_maps, &maps);
        }
        merge.stop();
        let mut accumulate = Measure::start("accumulate");
        let hashes: Vec<_> = account_maps
            .into_iter()
            .map(|(pubkey, (_, hash))| (pubkey, hash, 0))
            .collect();
        let ret = Self::accumulate_account_hashes(hashes);
        accumulate.stop();
        self.stats
            .delta_hash_scan_time_total_us
            .fetch_add(scan.as_us(), Ordering::Relaxed);
        self.stats
            .delta_hash_merge_time_total_us
            .fetch_add(merge.as_us(), Ordering::Relaxed);
        self.stats
            .delta_hash_accumulate_time_total_us
            .fetch_add(accumulate.as_us(), Ordering::Relaxed);
        self.stats.delta_hash_num.fetch_add(1, Ordering::Relaxed);
        ret
    }

    fn update_index(
        &self,
        slot: Slot,
        infos: Vec<AccountInfo>,
        accounts: &[(&Pubkey, &Account)],
    ) -> SlotList<AccountInfo> {
        let mut reclaims = SlotList::<AccountInfo>::with_capacity(infos.len() * 2);
        let index = self.accounts_index.read().unwrap();
        let inserts: Vec<_> = infos
            .into_iter()
            .zip(accounts.iter())
            .filter_map(|(info, pubkey_account)| {
                let pubkey = pubkey_account.0;
                index
                    .update(slot, pubkey, info, &mut reclaims)
                    .map(|info| (pubkey, info))
            })
            .collect();

        drop(index);
        if !inserts.is_empty() {
            let mut index = self.accounts_index.write().unwrap();
            for (pubkey, info) in inserts {
                index.insert(slot, pubkey, info, &mut reclaims);
            }
        }
        reclaims
    }

    fn remove_dead_accounts(
        &self,
        reclaims: SlotSlice<AccountInfo>,
        expected_slot: Option<Slot>,
    ) -> HashSet<Slot> {
        let storage = self.storage.read().unwrap();
        let mut dead_slots = HashSet::new();
        for (slot, account_info) in reclaims {
            if let Some(expected_slot) = expected_slot {
                assert_eq!(*slot, expected_slot);
            }
            if let Some(slot_storage) = storage.0.get(slot) {
                if let Some(store) = slot_storage.get(&account_info.store_id) {
                    assert_eq!(
                        *slot, store.slot,
                        "AccountDB::accounts_index corrupted. Storage should only point to one slot"
                    );
                    let count = store.remove_account();
                    if count == 0 {
                        dead_slots.insert(*slot);
                    }
                }
            }
        }

        dead_slots.retain(|slot| {
            if let Some(slot_storage) = storage.0.get(&slot) {
                for x in slot_storage.values() {
                    if x.count() != 0 {
                        return false;
                    }
                }
            }
            true
        });

        dead_slots
    }

    fn clean_dead_slots(&self, dead_slots: &HashSet<Slot>) {
        {
            let mut measure = Measure::start("clean_dead_slots-ms");
            let storage = self.storage.read().unwrap();
            let mut stores: Vec<Arc<AccountStorageEntry>> = vec![];
            for slot in dead_slots.iter() {
                if let Some(slot_storage) = storage.0.get(slot) {
                    for store in slot_storage.values() {
                        stores.push(store.clone());
                    }
                }
            }
            drop(storage);
            datapoint_debug!("clean_dead_slots", ("stores", stores.len(), i64));
            let slot_pubkeys: HashSet<(Slot, Pubkey)> = {
                self.thread_pool_clean.install(|| {
                    stores
                        .into_par_iter()
                        .map(|store| {
                            let accounts = store.accounts.accounts(0);
                            accounts
                                .into_iter()
                                .map(|account| (store.slot, account.meta.pubkey))
                                .collect::<HashSet<(Slot, Pubkey)>>()
                        })
                        .reduce(HashSet::new, |mut reduced, store_pubkeys| {
                            reduced.extend(store_pubkeys);
                            reduced
                        })
                })
            };
            let index = self.accounts_index.read().unwrap();
            for (_slot, pubkey) in slot_pubkeys {
                index.unref_from_storage(&pubkey);
            }
            drop(index);
            measure.stop();
            inc_new_counter_info!("clean_dead_slots-unref-ms", measure.as_ms() as usize);

            let mut index = self.accounts_index.write().unwrap();
            for slot in dead_slots.iter() {
                index.clean_dead_slot(*slot);
            }
        }
        {
            let mut bank_hashes = self.bank_hashes.write().unwrap();
            for slot in dead_slots.iter() {
                bank_hashes.remove(slot);
            }
        }
    }

    fn hash_accounts(
        &self,
        slot: Slot,
        accounts: &[(&Pubkey, &Account)],
        cluster_type: &ClusterType,
    ) -> Vec<Hash> {
        let mut stats = BankHashStats::default();
        let hashes: Vec<_> = accounts
            .iter()
            .map(|(pubkey, account)| {
                stats.update(account);
                Self::hash_account(slot, account, pubkey, cluster_type)
            })
            .collect();

        let mut bank_hashes = self.bank_hashes.write().unwrap();
        let slot_info = bank_hashes
            .entry(slot)
            .or_insert_with(BankHashInfo::default);
        slot_info.stats.merge(&stats);

        hashes
    }

    pub(crate) fn freeze_accounts(&mut self, ancestors: &Ancestors, account_pubkeys: &[Pubkey]) {
        for account_pubkey in account_pubkeys {
            if let Some((account, _slot)) = self.load_slow(ancestors, &account_pubkey) {
                let frozen_account_info = FrozenAccountInfo {
                    hash: Self::hash_frozen_account_data(&account),
                    lamports: account.lamports,
                };
                warn!(
                    "Account {} is now frozen at lamports={}, hash={}",
                    account_pubkey, frozen_account_info.lamports, frozen_account_info.hash
                );
                self.frozen_accounts
                    .insert(*account_pubkey, frozen_account_info);
            } else {
                panic!(
                    "Unable to freeze an account that does not exist: {}",
                    account_pubkey
                );
            }
        }
    }

    /// Cause a panic if frozen accounts would be affected by data in `accounts`
    fn assert_frozen_accounts(&self, accounts: &[(&Pubkey, &Account)]) {
        if self.frozen_accounts.is_empty() {
            return;
        }
        for (account_pubkey, account) in accounts.iter() {
            if let Some(frozen_account_info) = self.frozen_accounts.get(*account_pubkey) {
                if account.lamports < frozen_account_info.lamports {
                    FROZEN_ACCOUNT_PANIC.store(true, Ordering::Relaxed);
                    panic!(
                        "Frozen account {} modified.  Lamports decreased from {} to {}",
                        account_pubkey, frozen_account_info.lamports, account.lamports,
                    )
                }

                let hash = Self::hash_frozen_account_data(&account);
                if hash != frozen_account_info.hash {
                    FROZEN_ACCOUNT_PANIC.store(true, Ordering::Relaxed);
                    panic!(
                        "Frozen account {} modified.  Hash changed from {} to {}",
                        account_pubkey, frozen_account_info.hash, hash,
                    )
                }
            }
        }
    }

    /// Store the account update.
    pub fn store(&self, slot: Slot, accounts: &[(&Pubkey, &Account)]) {
        self.assert_frozen_accounts(accounts);
        let hashes = self.hash_accounts(
            slot,
            accounts,
            &self
                .cluster_type
                .expect("Cluster type must be set at initialization"),
        );
        self.store_with_hashes(slot, accounts, &hashes);
    }

    fn store_with_hashes(&self, slot: Slot, accounts: &[(&Pubkey, &Account)], hashes: &[Hash]) {
        let infos = self.store_accounts(slot, accounts, hashes);
        let reclaims = self.update_index(slot, infos, accounts);

        // A store for a single slot should:
        // 1) Only make "reclaims" for the same slot
        // 2) Should not cause any slots to be removed from the storage
        // database because
        //    a) this slot  has at least one account (the one being stored),
        //    b)From 1) we know no other slots are included in the "reclaims"
        //
        // From 1) and 2) we guarantee passing Some(slot), true is safe
        self.handle_reclaims(&reclaims, Some(slot), true);
    }

    pub fn add_root(&self, slot: Slot) {
        self.accounts_index.write().unwrap().add_root(slot)
    }

    pub fn get_snapshot_storages(&self, snapshot_slot: Slot) -> SnapshotStorages {
        let accounts_index = self.accounts_index.read().unwrap();
        let r_storage = self.storage.read().unwrap();
        r_storage
            .0
            .iter()
            .filter(|(slot, _slot_stores)| {
                **slot <= snapshot_slot && accounts_index.is_root(**slot)
            })
            .map(|(_slot, slot_stores)| {
                slot_stores
                    .values()
                    .filter(|x| x.has_accounts())
                    .cloned()
                    .collect()
            })
            .filter(|snapshot_storage: &SnapshotStorage| !snapshot_storage.is_empty())
            .collect()
    }

    fn merge<X>(dest: &mut HashMap<Pubkey, X>, source: &HashMap<Pubkey, X>)
    where
        X: Versioned + Clone,
    {
        for (key, source_item) in source.iter() {
            if let Some(dest_item) = dest.get(key) {
                if dest_item.version() > source_item.version() {
                    continue;
                }
            }
            dest.insert(*key, source_item.clone());
        }
    }

    pub fn generate_index(&self) {
        let mut accounts_index = self.accounts_index.write().unwrap();
        let storage = self.storage.read().unwrap();
        let mut slots: Vec<Slot> = storage.0.keys().cloned().collect();
        slots.sort();

        let mut last_log_update = Instant::now();
        for (index, slot) in slots.iter().enumerate() {
            let now = Instant::now();
            if now.duration_since(last_log_update).as_secs() >= 10 {
                info!("generating index: {}/{} slots...", index, slots.len());
                last_log_update = now;
            }

            let accumulator: Vec<HashMap<Pubkey, Vec<(u64, AccountInfo)>>> = self
                .scan_account_storage_inner(
                    *slot,
                    |stored_account: &StoredAccount,
                     store_id: AppendVecId,
                     accum: &mut HashMap<Pubkey, Vec<(u64, AccountInfo)>>| {
                        let account_info = AccountInfo {
                            store_id,
                            offset: stored_account.offset,
                            lamports: stored_account.account_meta.lamports,
                        };
                        let entry = accum
                            .entry(stored_account.meta.pubkey)
                            .or_insert_with(Vec::new);
                        entry.push((stored_account.meta.write_version, account_info));
                    },
                    &storage,
                );

            let mut accounts_map: HashMap<Pubkey, Vec<(u64, AccountInfo)>> = HashMap::new();
            for accumulator_entry in accumulator.iter() {
                for (pubkey, storage_entry) in accumulator_entry {
                    let entry = accounts_map.entry(*pubkey).or_insert_with(Vec::new);
                    entry.extend(storage_entry.iter().cloned());
                }
            }

            // Need to restore indexes even with older write versions which may
            // be shielding other accounts. When they are then purged, the
            // original non-shielded account value will be visible when the account
            // is restored from the append-vec
            if !accumulator.is_empty() {
                let mut _reclaims: Vec<(u64, AccountInfo)> = vec![];
                for (pubkey, account_infos) in accounts_map.iter_mut() {
                    account_infos.sort_by(|a, b| a.0.cmp(&b.0));
                    for (_, account_info) in account_infos {
                        accounts_index.insert(*slot, pubkey, account_info.clone(), &mut _reclaims);
                    }
                }
            }
        }
        // Need to add these last, otherwise older updates will be cleaned
        for slot in slots {
            accounts_index.add_root(slot);
        }

        let mut counts = HashMap::new();
        for slot_list in accounts_index.account_maps.values() {
            for (_slot, account_entry) in slot_list.1.read().unwrap().iter() {
                *counts.entry(account_entry.store_id).or_insert(0) += 1;
            }
        }
        for slot_stores in storage.0.values() {
            for (id, store) in slot_stores {
                if let Some(count) = counts.get(&id) {
                    trace!(
                        "id: {} setting count: {} cur: {}",
                        id,
                        count,
                        store.count_and_status.read().unwrap().0
                    );
                    store.count_and_status.write().unwrap().0 = *count;
                } else {
                    trace!("id: {} clearing count", id);
                    store.count_and_status.write().unwrap().0 = 0;
                }
                store
                    .approx_store_count
                    .store(store.accounts.accounts(0).len(), Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn print_accounts_stats(&self, label: &'static str) {
        self.print_index(label);
        self.print_count_and_status(label);
    }

    fn print_index(&self, label: &'static str) {
        let mut roots: Vec<_> = self
            .accounts_index
            .read()
            .unwrap()
            .roots
            .iter()
            .cloned()
            .collect();
        roots.sort();
        info!("{}: accounts_index roots: {:?}", label, roots,);
        for (pubkey, list) in &self.accounts_index.read().unwrap().account_maps {
            info!("  key: {}", pubkey);
            info!("      slots: {:?}", *list.1.read().unwrap());
        }
    }

    fn print_count_and_status(&self, label: &'static str) {
        let storage = self.storage.read().unwrap();
        let mut slots: Vec<_> = storage.0.keys().cloned().collect();
        slots.sort();
        info!("{}: count_and status for {} slots:", label, slots.len());
        for slot in &slots {
            let slot_stores = storage.0.get(slot).unwrap();

            let mut ids: Vec<_> = slot_stores.keys().cloned().collect();
            ids.sort();
            for id in &ids {
                let entry = slot_stores.get(id).unwrap();
                info!(
                    "  slot: {} id: {} count_and_status: {:?} approx_store_count: {} len: {} capacity: {}",
                    slot,
                    id,
                    *entry.count_and_status.read().unwrap(),
                    entry.approx_store_count.load(Ordering::Relaxed),
                    entry.accounts.len(),
                    entry.accounts.capacity(),
                );
            }
        }
    }

    #[cfg(test)]
    pub fn get_append_vec_id(&self, pubkey: &Pubkey, slot: Slot) -> Option<AppendVecId> {
        let ancestors = vec![(slot, 1)].into_iter().collect();
        let accounts_index = self.accounts_index.read().unwrap();
        let result = accounts_index.get(&pubkey, Some(&ancestors), None);
        result.map(|(list, index)| list[index].1.store_id)
    }
}

#[cfg(test)]
pub mod tests {
    // TODO: all the bank tests are bank specific, issue: 2194
    use super::*;
    use crate::{accounts_index::RefCount, append_vec::AccountMeta};
    use assert_matches::assert_matches;
    use rand::{thread_rng, Rng};
    use solana_sdk::{account::Account, hash::HASH_BYTES};
    use std::{fs, str::FromStr};

    fn linear_ancestors(end_slot: u64) -> Ancestors {
        let mut ancestors: Ancestors = vec![(0, 0)].into_iter().collect();
        for i in 1..end_slot {
            ancestors.insert(i, (i - 1) as usize);
        }
        ancestors
    }

    #[test]
    fn test_accountsdb_add_root() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::default();
        let account0 = Account::new(1, 0, &key);

        db.store(0, &[(&key, &account0)]);
        db.add_root(0);
        let ancestors = vec![(1, 1)].into_iter().collect();
        assert_eq!(db.load_slow(&ancestors, &key), Some((account0, 0)));
    }

    #[test]
    fn test_accountsdb_latest_ancestor() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::default();
        let account0 = Account::new(1, 0, &key);

        db.store(0, &[(&key, &account0)]);

        let account1 = Account::new(0, 0, &key);
        db.store(1, &[(&key, &account1)]);

        let ancestors = vec![(1, 1)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account1);

        let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account1);

        let accounts: Vec<Account> =
            db.scan_accounts(&ancestors, |accounts: &mut Vec<Account>, option| {
                if let Some(data) = option {
                    accounts.push(data.1);
                }
            });
        assert_eq!(accounts, vec![account1]);
    }

    #[test]
    fn test_accountsdb_latest_ancestor_with_root() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::default();
        let account0 = Account::new(1, 0, &key);

        db.store(0, &[(&key, &account0)]);

        let account1 = Account::new(0, 0, &key);
        db.store(1, &[(&key, &account1)]);
        db.add_root(0);

        let ancestors = vec![(1, 1)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account1);

        let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account1);
    }

    #[test]
    fn test_accountsdb_root_one_slot() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let account0 = Account::new(1, 0, &key);

        // store value 1 in the "root", i.e. db zero
        db.store(0, &[(&key, &account0)]);

        // now we have:
        //
        //                       root0 -> key.lamports==1
        //                        / \
        //                       /   \
        //  key.lamports==0 <- slot1    \
        //                             slot2 -> key.lamports==1
        //                                       (via root0)

        // store value 0 in one child
        let account1 = Account::new(0, 0, &key);
        db.store(1, &[(&key, &account1)]);

        // masking accounts is done at the Accounts level, at accountsDB we see
        // original account (but could also accept "None", which is implemented
        // at the Accounts level)
        let ancestors = vec![(0, 0), (1, 1)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account1);

        // we should see 1 token in slot 2
        let ancestors = vec![(0, 0), (2, 2)].into_iter().collect();
        assert_eq!(&db.load_slow(&ancestors, &key).unwrap().0, &account0);

        db.add_root(0);

        let ancestors = vec![(1, 1)].into_iter().collect();
        assert_eq!(db.load_slow(&ancestors, &key), Some((account1, 1)));
        let ancestors = vec![(2, 2)].into_iter().collect();
        assert_eq!(db.load_slow(&ancestors, &key), Some((account0, 0))); // original value
    }

    #[test]
    fn test_accountsdb_add_root_many() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&db, &mut pubkeys, 0, 100, 0, 0);
        for _ in 1..100 {
            let idx = thread_rng().gen_range(0, 99);
            let ancestors = vec![(0, 0)].into_iter().collect();
            let account = db.load_slow(&ancestors, &pubkeys[idx]).unwrap();
            let mut default_account = Account::default();
            default_account.lamports = (idx + 1) as u64;
            assert_eq!((default_account, 0), account);
        }

        db.add_root(0);

        // check that all the accounts appear with a new root
        for _ in 1..100 {
            let idx = thread_rng().gen_range(0, 99);
            let ancestors = vec![(0, 0)].into_iter().collect();
            let account0 = db.load_slow(&ancestors, &pubkeys[idx]).unwrap();
            let ancestors = vec![(1, 1)].into_iter().collect();
            let account1 = db.load_slow(&ancestors, &pubkeys[idx]).unwrap();
            let mut default_account = Account::default();
            default_account.lamports = (idx + 1) as u64;
            assert_eq!(&default_account, &account0.0);
            assert_eq!(&default_account, &account1.0);
        }
    }

    #[test]
    fn test_accountsdb_count_stores() {
        solana_logger::setup();
        let db = AccountsDB::new_single();

        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&db, &mut pubkeys, 0, 2, DEFAULT_FILE_SIZE as usize / 3, 0);
        assert!(check_storage(&db, 0, 2));

        let pubkey = Pubkey::new_rand();
        let account = Account::new(1, DEFAULT_FILE_SIZE as usize / 3, &pubkey);
        db.store(1, &[(&pubkey, &account)]);
        db.store(1, &[(&pubkeys[0], &account)]);
        {
            let stores = db.storage.read().unwrap();
            let slot_0_stores = &stores.0.get(&0).unwrap();
            let slot_1_stores = &stores.0.get(&1).unwrap();
            assert_eq!(slot_0_stores.len(), 1);
            assert_eq!(slot_1_stores.len(), 1);
            assert_eq!(slot_0_stores[&0].count(), 2);
            assert_eq!(slot_1_stores[&1].count(), 2);
            assert_eq!(slot_0_stores[&0].approx_stored_count(), 2);
            assert_eq!(slot_1_stores[&1].approx_stored_count(), 2);
        }

        // adding root doesn't change anything
        db.add_root(1);
        {
            let stores = db.storage.read().unwrap();
            let slot_0_stores = &stores.0.get(&0).unwrap();
            let slot_1_stores = &stores.0.get(&1).unwrap();
            assert_eq!(slot_0_stores.len(), 1);
            assert_eq!(slot_1_stores.len(), 1);
            assert_eq!(slot_0_stores[&0].count(), 2);
            assert_eq!(slot_1_stores[&1].count(), 2);
            assert_eq!(slot_0_stores[&0].approx_stored_count(), 2);
            assert_eq!(slot_1_stores[&1].approx_stored_count(), 2);
        }

        // overwrite old rooted account version; only the slot_0_stores.count() should be
        // decremented
        db.store(2, &[(&pubkeys[0], &account)]);
        db.clean_accounts(None);
        {
            let stores = db.storage.read().unwrap();
            let slot_0_stores = &stores.0.get(&0).unwrap();
            let slot_1_stores = &stores.0.get(&1).unwrap();
            assert_eq!(slot_0_stores.len(), 1);
            assert_eq!(slot_1_stores.len(), 1);
            assert_eq!(slot_0_stores[&0].count(), 1);
            assert_eq!(slot_1_stores[&1].count(), 2);
            assert_eq!(slot_0_stores[&0].approx_stored_count(), 2);
            assert_eq!(slot_1_stores[&1].approx_stored_count(), 2);
        }
    }

    #[test]
    fn test_accounts_unsquashed() {
        let key = Pubkey::default();

        // 1 token in the "root", i.e. db zero
        let db0 = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let account0 = Account::new(1, 0, &key);
        db0.store(0, &[(&key, &account0)]);

        // 0 lamports in the child
        let account1 = Account::new(0, 0, &key);
        db0.store(1, &[(&key, &account1)]);

        // masking accounts is done at the Accounts level, at accountsDB we see
        // original account
        let ancestors = vec![(0, 0), (1, 1)].into_iter().collect();
        assert_eq!(db0.load_slow(&ancestors, &key), Some((account1, 1)));
        let ancestors = vec![(0, 0)].into_iter().collect();
        assert_eq!(db0.load_slow(&ancestors, &key), Some((account0, 0)));
    }

    #[test]
    fn test_remove_unrooted_slot() {
        let unrooted_slot = 9;
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::default();
        let account0 = Account::new(1, 0, &key);
        let ancestors: HashMap<_, _> = vec![(unrooted_slot, 1)].into_iter().collect();
        db.store(unrooted_slot, &[(&key, &account0)]);
        db.bank_hashes
            .write()
            .unwrap()
            .insert(unrooted_slot, BankHashInfo::default());
        assert!(db
            .accounts_index
            .read()
            .unwrap()
            .get(&key, Some(&ancestors), None)
            .is_some());
        assert_load_account(&db, unrooted_slot, key, 1);

        // Purge the slot
        db.remove_unrooted_slot(unrooted_slot);
        assert!(db.load_slow(&ancestors, &key).is_none());
        assert!(db.bank_hashes.read().unwrap().get(&unrooted_slot).is_none());
        assert!(db.storage.read().unwrap().0.get(&unrooted_slot).is_none());
        assert!(db
            .accounts_index
            .read()
            .unwrap()
            .account_maps
            .get(&key)
            .map(|pubkey_entry| pubkey_entry.1.read().unwrap().is_empty())
            .unwrap_or(true));
        assert!(db
            .accounts_index
            .read()
            .unwrap()
            .get(&key, Some(&ancestors), None)
            .is_none());

        // Test we can store for the same slot again and get the right information
        let account0 = Account::new(2, 0, &key);
        db.store(unrooted_slot, &[(&key, &account0)]);
        assert_load_account(&db, unrooted_slot, key, 2);
    }

    #[test]
    fn test_remove_unrooted_slot_snapshot() {
        let unrooted_slot = 9;
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::new_rand();
        let account0 = Account::new(1, 0, &key);
        db.store(unrooted_slot, &[(&key, &account0)]);

        // Purge the slot
        db.remove_unrooted_slot(unrooted_slot);

        // Add a new root
        let key2 = Pubkey::new_rand();
        let new_root = unrooted_slot + 1;
        db.store(new_root, &[(&key2, &account0)]);
        db.add_root(new_root);

        // Simulate reconstruction from snapshot
        let db = reconstruct_accounts_db_via_serialization(&db, new_root);

        // Check root account exists
        assert_load_account(&db, new_root, key2, 1);

        // Check purged account stays gone
        let unrooted_slot_ancestors: HashMap<_, _> = vec![(unrooted_slot, 1)].into_iter().collect();
        assert!(db.load_slow(&unrooted_slot_ancestors, &key).is_none());
    }

    fn create_account(
        accounts: &AccountsDB,
        pubkeys: &mut Vec<Pubkey>,
        slot: Slot,
        num: usize,
        space: usize,
        num_vote: usize,
    ) {
        let ancestors = vec![(slot, 0)].into_iter().collect();
        for t in 0..num {
            let pubkey = Pubkey::new_rand();
            let account = Account::new((t + 1) as u64, space, &Account::default().owner);
            pubkeys.push(pubkey);
            assert!(accounts.load_slow(&ancestors, &pubkey).is_none());
            accounts.store(slot, &[(&pubkey, &account)]);
        }
        for t in 0..num_vote {
            let pubkey = Pubkey::new_rand();
            let account = Account::new((num + t + 1) as u64, space, &solana_vote_program::id());
            pubkeys.push(pubkey);
            let ancestors = vec![(slot, 0)].into_iter().collect();
            assert!(accounts.load_slow(&ancestors, &pubkey).is_none());
            accounts.store(slot, &[(&pubkey, &account)]);
        }
    }

    fn update_accounts(accounts: &AccountsDB, pubkeys: &[Pubkey], slot: Slot, range: usize) {
        for _ in 1..1000 {
            let idx = thread_rng().gen_range(0, range);
            let ancestors = vec![(slot, 0)].into_iter().collect();
            if let Some((mut account, _)) = accounts.load_slow(&ancestors, &pubkeys[idx]) {
                account.lamports += 1;
                accounts.store(slot, &[(&pubkeys[idx], &account)]);
                if account.lamports == 0 {
                    let ancestors = vec![(slot, 0)].into_iter().collect();
                    assert!(accounts.load_slow(&ancestors, &pubkeys[idx]).is_none());
                } else {
                    let mut default_account = Account::default();
                    default_account.lamports = account.lamports;
                    assert_eq!(default_account, account);
                }
            }
        }
    }

    fn check_storage(accounts: &AccountsDB, slot: Slot, count: usize) -> bool {
        let storage = accounts.storage.read().unwrap();
        assert_eq!(storage.0[&slot].len(), 1);
        let slot_storage = storage.0.get(&slot).unwrap();
        let mut total_count: usize = 0;
        for store in slot_storage.values() {
            assert_eq!(store.status(), AccountStorageStatus::Available);
            total_count += store.count();
        }
        assert_eq!(total_count, count);
        let (expected_store_count, actual_store_count): (usize, usize) = (
            slot_storage.values().map(|s| s.approx_stored_count()).sum(),
            slot_storage
                .values()
                .map(|s| s.accounts.accounts(0).len())
                .sum(),
        );
        assert_eq!(expected_store_count, actual_store_count);
        total_count == count
    }

    fn check_accounts(
        accounts: &AccountsDB,
        pubkeys: &[Pubkey],
        slot: Slot,
        num: usize,
        count: usize,
    ) {
        let ancestors = vec![(slot, 0)].into_iter().collect();
        for _ in 0..num {
            let idx = thread_rng().gen_range(0, num);
            let account = accounts.load_slow(&ancestors, &pubkeys[idx]);
            let account1 = Some((
                Account::new((idx + count) as u64, 0, &Account::default().owner),
                slot,
            ));
            assert_eq!(account, account1);
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn modify_accounts(
        accounts: &AccountsDB,
        pubkeys: &[Pubkey],
        slot: Slot,
        num: usize,
        count: usize,
    ) {
        for idx in 0..num {
            let account = Account::new((idx + count) as u64, 0, &Account::default().owner);
            accounts.store(slot, &[(&pubkeys[idx], &account)]);
        }
    }

    #[test]
    fn test_account_one() {
        let (_accounts_dirs, paths) = get_temp_accounts_paths(1).unwrap();
        let db = AccountsDB::new(paths, &ClusterType::Development);
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&db, &mut pubkeys, 0, 1, 0, 0);
        let ancestors = vec![(0, 0)].into_iter().collect();
        let account = db.load_slow(&ancestors, &pubkeys[0]).unwrap();
        let mut default_account = Account::default();
        default_account.lamports = 1;
        assert_eq!((default_account, 0), account);
    }

    #[test]
    fn test_account_many() {
        let (_accounts_dirs, paths) = get_temp_accounts_paths(2).unwrap();
        let db = AccountsDB::new(paths, &ClusterType::Development);
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&db, &mut pubkeys, 0, 100, 0, 0);
        check_accounts(&db, &pubkeys, 0, 100, 1);
    }

    #[test]
    fn test_account_update() {
        let accounts = AccountsDB::new_single();
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&accounts, &mut pubkeys, 0, 100, 0, 0);
        update_accounts(&accounts, &pubkeys, 0, 99);
        assert_eq!(check_storage(&accounts, 0, 100), true);
    }

    #[test]
    fn test_account_grow_many() {
        let (_accounts_dir, paths) = get_temp_accounts_paths(2).unwrap();
        let size = 4096;
        let accounts = AccountsDB::new_sized(paths, size);
        let mut keys = vec![];
        for i in 0..9 {
            let key = Pubkey::new_rand();
            let account = Account::new(i + 1, size as usize / 4, &key);
            accounts.store(0, &[(&key, &account)]);
            keys.push(key);
        }
        let ancestors = vec![(0, 0)].into_iter().collect();
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                accounts.load_slow(&ancestors, &key).unwrap().0.lamports,
                (i as u64) + 1
            );
        }

        let mut append_vec_histogram = HashMap::new();
        for storage in accounts
            .storage
            .read()
            .unwrap()
            .0
            .values()
            .flat_map(|x| x.values())
        {
            *append_vec_histogram.entry(storage.slot).or_insert(0) += 1;
        }
        for count in append_vec_histogram.values() {
            assert!(*count >= 2);
        }
    }

    #[test]
    fn test_account_grow() {
        let accounts = AccountsDB::new_single();

        let count = [0, 1];
        let status = [AccountStorageStatus::Available, AccountStorageStatus::Full];
        let pubkey1 = Pubkey::new_rand();
        let account1 = Account::new(1, DEFAULT_FILE_SIZE as usize / 2, &pubkey1);
        accounts.store(0, &[(&pubkey1, &account1)]);
        {
            let stores = accounts.storage.read().unwrap();
            assert_eq!(stores.0.len(), 1);
            assert_eq!(stores.0[&0][&0].count(), 1);
            assert_eq!(stores.0[&0][&0].status(), AccountStorageStatus::Available);
        }

        let pubkey2 = Pubkey::new_rand();
        let account2 = Account::new(1, DEFAULT_FILE_SIZE as usize / 2, &pubkey2);
        accounts.store(0, &[(&pubkey2, &account2)]);
        {
            let stores = accounts.storage.read().unwrap();
            assert_eq!(stores.0.len(), 1);
            assert_eq!(stores.0[&0].len(), 2);
            assert_eq!(stores.0[&0][&0].count(), 1);
            assert_eq!(stores.0[&0][&0].status(), AccountStorageStatus::Full);
            assert_eq!(stores.0[&0][&1].count(), 1);
            assert_eq!(stores.0[&0][&1].status(), AccountStorageStatus::Available);
        }
        let ancestors = vec![(0, 0)].into_iter().collect();
        assert_eq!(
            accounts.load_slow(&ancestors, &pubkey1).unwrap().0,
            account1
        );
        assert_eq!(
            accounts.load_slow(&ancestors, &pubkey2).unwrap().0,
            account2
        );

        // lots of stores, but 3 storages should be enough for everything
        for i in 0..25 {
            let index = i % 2;
            accounts.store(0, &[(&pubkey1, &account1)]);
            {
                let stores = accounts.storage.read().unwrap();
                assert_eq!(stores.0.len(), 1);
                assert_eq!(stores.0[&0].len(), 3);
                assert_eq!(stores.0[&0][&0].count(), count[index]);
                assert_eq!(stores.0[&0][&0].status(), status[0]);
                assert_eq!(stores.0[&0][&1].count(), 1);
                assert_eq!(stores.0[&0][&1].status(), status[1]);
                assert_eq!(stores.0[&0][&2].count(), count[index ^ 1]);
                assert_eq!(stores.0[&0][&2].status(), status[0]);
            }
            let ancestors = vec![(0, 0)].into_iter().collect();
            assert_eq!(
                accounts.load_slow(&ancestors, &pubkey1).unwrap().0,
                account1
            );
            assert_eq!(
                accounts.load_slow(&ancestors, &pubkey2).unwrap().0,
                account2
            );
        }
    }

    #[test]
    fn test_purge_slot_not_root() {
        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&accounts, &mut pubkeys, 0, 1, 0, 0);
        let ancestors = vec![(0, 0)].into_iter().collect();
        assert!(accounts.load_slow(&ancestors, &pubkeys[0]).is_some());
        accounts.purge_slot(0);
        assert!(accounts.load_slow(&ancestors, &pubkeys[0]).is_none());
    }

    #[test]
    fn test_purge_slot_after_root() {
        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(&accounts, &mut pubkeys, 0, 1, 0, 0);
        let ancestors = vec![(0, 0)].into_iter().collect();
        accounts.add_root(0);
        accounts.purge_slot(0);
        assert!(accounts.load_slow(&ancestors, &pubkeys[0]).is_some());
    }

    #[test]
    fn test_lazy_gc_slot() {
        solana_logger::setup();
        //This test is pedantic
        //A slot is purged when a non root bank is cleaned up.  If a slot is behind root but it is
        //not root, it means we are retaining dead banks.
        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey = Pubkey::new_rand();
        let account = Account::new(1, 0, &Account::default().owner);
        //store an account
        accounts.store(0, &[(&pubkey, &account)]);
        let ancestors = vec![(0, 0)].into_iter().collect();
        let id = {
            let index = accounts.accounts_index.read().unwrap();
            let (list, idx) = index.get(&pubkey, Some(&ancestors), None).unwrap();
            list[idx].1.store_id
        };
        accounts.add_root(1);

        //slot is still there, since gc is lazy
        assert!(accounts.storage.read().unwrap().0[&0].get(&id).is_some());

        //store causes clean
        accounts.store(1, &[(&pubkey, &account)]);

        //slot is gone
        accounts.print_accounts_stats("pre-clean");
        accounts.clean_accounts(None);
        assert!(accounts.storage.read().unwrap().0.get(&0).is_none());

        //new value is there
        let ancestors = vec![(1, 1)].into_iter().collect();
        assert_eq!(accounts.load_slow(&ancestors, &pubkey), Some((account, 1)));
    }

    impl AccountsDB {
        fn alive_account_count_in_store(&self, slot: Slot) -> usize {
            let storage = self.storage.read().unwrap();

            let slot_storage = storage.0.get(&slot);
            if let Some(slot_storage) = slot_storage {
                slot_storage.values().map(|store| store.count()).sum()
            } else {
                0
            }
        }

        fn all_account_count_in_append_vec(&self, slot: Slot) -> usize {
            let storage = self.storage.read().unwrap();

            let slot_storage = storage.0.get(&slot);
            if let Some(slot_storage) = slot_storage {
                let count = slot_storage
                    .values()
                    .map(|store| store.accounts.accounts(0).len())
                    .sum();
                let stored_count: usize = slot_storage
                    .values()
                    .map(|store| store.approx_stored_count())
                    .sum();
                assert_eq!(stored_count, count);
                count
            } else {
                0
            }
        }

        fn ref_count_for_pubkey(&self, pubkey: &Pubkey) -> RefCount {
            self.accounts_index
                .read()
                .unwrap()
                .ref_count_from_storage(&pubkey)
        }

        fn uncleaned_root_count(&self) -> usize {
            self.accounts_index.read().unwrap().uncleaned_roots.len()
        }
    }

    #[test]
    fn test_clean_old_with_normal_account() {
        solana_logger::setup();

        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey = Pubkey::new_rand();
        let account = Account::new(1, 0, &Account::default().owner);
        //store an account
        accounts.store(0, &[(&pubkey, &account)]);
        accounts.store(1, &[(&pubkey, &account)]);

        // simulate slots are rooted after while
        accounts.add_root(0);
        accounts.add_root(1);

        //even if rooted, old state isn't cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 1);
        assert_eq!(accounts.alive_account_count_in_store(1), 1);

        accounts.clean_accounts(None);

        //now old state is cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 0);
        assert_eq!(accounts.alive_account_count_in_store(1), 1);
    }

    #[test]
    fn test_clean_old_with_zero_lamport_account() {
        solana_logger::setup();

        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey1 = Pubkey::new_rand();
        let pubkey2 = Pubkey::new_rand();
        let normal_account = Account::new(1, 0, &Account::default().owner);
        let zero_account = Account::new(0, 0, &Account::default().owner);
        //store an account
        accounts.store(0, &[(&pubkey1, &normal_account)]);
        accounts.store(1, &[(&pubkey1, &zero_account)]);
        accounts.store(0, &[(&pubkey2, &normal_account)]);
        accounts.store(1, &[(&pubkey2, &normal_account)]);

        //simulate slots are rooted after while
        accounts.add_root(0);
        accounts.add_root(1);

        //even if rooted, old state isn't cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 2);
        assert_eq!(accounts.alive_account_count_in_store(1), 2);

        accounts.clean_accounts(None);

        //still old state behind zero-lamport account isn't cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 1);
        assert_eq!(accounts.alive_account_count_in_store(1), 2);
    }

    #[test]
    fn test_clean_old_with_both_normal_and_zero_lamport_accounts() {
        solana_logger::setup();

        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey1 = Pubkey::new_rand();
        let pubkey2 = Pubkey::new_rand();
        let normal_account = Account::new(1, 0, &Account::default().owner);
        let zero_account = Account::new(0, 0, &Account::default().owner);

        //store an account
        accounts.store(0, &[(&pubkey1, &normal_account)]);
        accounts.store(0, &[(&pubkey1, &normal_account)]);
        accounts.store(1, &[(&pubkey1, &zero_account)]);
        accounts.store(0, &[(&pubkey2, &normal_account)]);
        accounts.store(2, &[(&pubkey2, &normal_account)]);

        //simulate slots are rooted after while
        accounts.add_root(0);
        accounts.add_root(1);
        accounts.add_root(2);

        //even if rooted, old state isn't cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 2);
        assert_eq!(accounts.alive_account_count_in_store(1), 1);
        assert_eq!(accounts.alive_account_count_in_store(2), 1);

        accounts.clean_accounts(None);

        //both zero lamport and normal accounts are cleaned up
        assert_eq!(accounts.alive_account_count_in_store(0), 0);
        // The only store to slot 1 was a zero lamport account, should
        // be purged by zero-lamport cleaning logic because slot 1 is
        // rooted
        assert_eq!(accounts.alive_account_count_in_store(1), 0);
        assert_eq!(accounts.alive_account_count_in_store(2), 1);

        // Pubkey 1, a zero lamport account, should no longer exist in accounts index
        // because it has been removed
        assert!(accounts
            .accounts_index
            .read()
            .unwrap()
            .get(&pubkey1, None, None)
            .is_none());
    }

    #[test]
    fn test_uncleaned_roots_with_account() {
        solana_logger::setup();

        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey = Pubkey::new_rand();
        let account = Account::new(1, 0, &Account::default().owner);
        //store an account
        accounts.store(0, &[(&pubkey, &account)]);
        assert_eq!(accounts.uncleaned_root_count(), 0);

        // simulate slots are rooted after while
        accounts.add_root(0);
        assert_eq!(accounts.uncleaned_root_count(), 1);

        //now uncleaned roots are cleaned up
        accounts.clean_accounts(None);
        assert_eq!(accounts.uncleaned_root_count(), 0);
    }

    #[test]
    fn test_uncleaned_roots_with_no_account() {
        solana_logger::setup();

        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);

        assert_eq!(accounts.uncleaned_root_count(), 0);

        // simulate slots are rooted after while
        accounts.add_root(0);
        assert_eq!(accounts.uncleaned_root_count(), 1);

        //now uncleaned roots are cleaned up
        accounts.clean_accounts(None);
        assert_eq!(accounts.uncleaned_root_count(), 0);
    }

    #[test]
    fn test_accounts_db_serialize1() {
        solana_logger::setup();
        let accounts = AccountsDB::new_single();
        let mut pubkeys: Vec<Pubkey> = vec![];

        // Create 100 accounts in slot 0
        create_account(&accounts, &mut pubkeys, 0, 100, 0, 0);
        accounts.clean_accounts(None);
        check_accounts(&accounts, &pubkeys, 0, 100, 1);

        // do some updates to those accounts and re-check
        modify_accounts(&accounts, &pubkeys, 0, 100, 2);
        assert_eq!(check_storage(&accounts, 0, 100), true);
        check_accounts(&accounts, &pubkeys, 0, 100, 2);
        accounts.add_root(0);

        let mut pubkeys1: Vec<Pubkey> = vec![];

        // CREATE SLOT 1
        let latest_slot = 1;

        // Modify the first 10 of the slot 0 accounts as updates in slot 1
        modify_accounts(&accounts, &pubkeys, latest_slot, 10, 3);

        // Create 10 new accounts in slot 1
        create_account(&accounts, &mut pubkeys1, latest_slot, 10, 0, 0);

        // Store a lamports=0 account in slot 1. Slot 1 should now have
        // 10 + 10 + 1 = 21 accounts
        let account = Account::new(0, 0, &Account::default().owner);
        accounts.store(latest_slot, &[(&pubkeys[30], &account)]);
        accounts.add_root(latest_slot);
        assert!(check_storage(&accounts, 1, 21));

        // CREATE SLOT 2
        let latest_slot = 2;
        let mut pubkeys2: Vec<Pubkey> = vec![];

        // Modify original slot 0 accounts in slot 2
        modify_accounts(&accounts, &pubkeys, latest_slot, 20, 4);
        accounts.clean_accounts(None);

        // Create 10 new accounts in slot 2
        create_account(&accounts, &mut pubkeys2, latest_slot, 10, 0, 0);

        // Store a lamports=0 account in slot 2. Slot 2 should now have
        // 10 + 10 + 10 + 1 = 31 accounts
        let account = Account::new(0, 0, &Account::default().owner);
        accounts.store(latest_slot, &[(&pubkeys[31], &account)]);
        accounts.add_root(latest_slot);
        assert!(check_storage(&accounts, 2, 31));

        accounts.clean_accounts(None);
        // The first 20 accounts have been modified in slot 2, so only
        // 80 accounts left in slot 0.
        assert!(check_storage(&accounts, 0, 80));
        // 10 of the 21 accounts have been modified in slot 2, so only 11
        // accounts left in slot 1.
        assert!(check_storage(&accounts, 1, 11));
        assert!(check_storage(&accounts, 2, 31));

        let daccounts = reconstruct_accounts_db_via_serialization(&accounts, latest_slot);

        assert_eq!(
            daccounts.write_version.load(Ordering::Relaxed),
            accounts.write_version.load(Ordering::Relaxed)
        );

        assert_eq!(
            daccounts.next_id.load(Ordering::Relaxed),
            accounts.next_id.load(Ordering::Relaxed)
        );

        // Get the hash for the latest slot, which should be the only hash in the
        // bank_hashes map on the deserialized AccountsDb
        assert_eq!(daccounts.bank_hashes.read().unwrap().len(), 2);
        assert_eq!(
            daccounts.bank_hashes.read().unwrap().get(&latest_slot),
            accounts.bank_hashes.read().unwrap().get(&latest_slot)
        );

        daccounts.print_count_and_status("daccounts");

        // Don't check the first 35 accounts which have not been modified on slot 0
        check_accounts(&daccounts, &pubkeys[35..], 0, 65, 37);
        check_accounts(&daccounts, &pubkeys1, 1, 10, 1);
        assert!(check_storage(&daccounts, 0, 100));
        assert!(check_storage(&daccounts, 1, 21));
        assert!(check_storage(&daccounts, 2, 31));

        let ancestors = linear_ancestors(latest_slot);
        assert_eq!(
            daccounts.update_accounts_hash(latest_slot, &ancestors),
            accounts.update_accounts_hash(latest_slot, &ancestors)
        );
    }

    fn assert_load_account(
        accounts: &AccountsDB,
        slot: Slot,
        pubkey: Pubkey,
        expected_lamports: u64,
    ) {
        let ancestors = vec![(slot, 0)].into_iter().collect();
        let (account, slot) = accounts.load_slow(&ancestors, &pubkey).unwrap();
        assert_eq!((account.lamports, slot), (expected_lamports, slot));
    }

    fn assert_not_load_account(accounts: &AccountsDB, slot: Slot, pubkey: Pubkey) {
        let ancestors = vec![(slot, 0)].into_iter().collect();
        assert!(accounts.load_slow(&ancestors, &pubkey).is_none());
    }

    fn reconstruct_accounts_db_via_serialization(accounts: &AccountsDB, slot: Slot) -> AccountsDB {
        let daccounts =
            crate::serde_snapshot::reconstruct_accounts_db_via_serialization(accounts, slot);
        daccounts.print_count_and_status("daccounts");
        daccounts
    }

    fn assert_no_stores(accounts: &AccountsDB, slot: Slot) {
        let stores = accounts.storage.read().unwrap();
        info!("{:?}", stores.0.get(&slot));
        assert!(stores.0.get(&slot).is_none() || stores.0.get(&slot).unwrap().is_empty());
    }

    #[test]
    fn test_accounts_db_purge_keep_live() {
        solana_logger::setup();
        let some_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);
        let pubkey = Pubkey::new_rand();

        let account2 = Account::new(some_lamport, no_data, &owner);
        let pubkey2 = Pubkey::new_rand();

        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let accounts = AccountsDB::new_single();
        accounts.add_root(0);

        let mut current_slot = 1;
        accounts.store(current_slot, &[(&pubkey, &account)]);

        // Store another live account to slot 1 which will prevent any purge
        // since the store count will not be zero
        accounts.store(current_slot, &[(&pubkey2, &account2)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&pubkey, &zero_lamport_account)]);
        accounts.add_root(current_slot);

        assert_load_account(&accounts, current_slot, pubkey, zero_lamport);

        current_slot += 1;
        accounts.add_root(current_slot);

        accounts.print_accounts_stats("pre_purge");

        accounts.clean_accounts(None);

        accounts.print_accounts_stats("post_purge");

        // Make sure the index is not touched
        assert_eq!(
            accounts
                .accounts_index
                .read()
                .unwrap()
                .account_maps
                .get(&pubkey)
                .unwrap()
                .1
                .read()
                .unwrap()
                .len(),
            2
        );

        // slot 1 & 2 should have stores
        check_storage(&accounts, 1, 2);
        check_storage(&accounts, 2, 1);
    }

    #[test]
    fn test_accounts_db_purge1() {
        solana_logger::setup();
        let some_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);
        let pubkey = Pubkey::new_rand();

        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let accounts = AccountsDB::new_single();
        accounts.add_root(0);

        let mut current_slot = 1;
        accounts.set_hash(current_slot, current_slot - 1);
        accounts.store(current_slot, &[(&pubkey, &account)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.set_hash(current_slot, current_slot - 1);
        accounts.store(current_slot, &[(&pubkey, &zero_lamport_account)]);
        accounts.add_root(current_slot);

        assert_load_account(&accounts, current_slot, pubkey, zero_lamport);

        // Otherwise slot 2 will not be removed
        current_slot += 1;
        accounts.set_hash(current_slot, current_slot - 1);
        accounts.add_root(current_slot);

        accounts.print_accounts_stats("pre_purge");

        let ancestors = linear_ancestors(current_slot);
        info!("ancestors: {:?}", ancestors);
        let hash = accounts.update_accounts_hash(current_slot, &ancestors);

        accounts.clean_accounts(None);

        assert_eq!(
            accounts.update_accounts_hash(current_slot, &ancestors),
            hash
        );

        accounts.print_accounts_stats("post_purge");

        // Make sure the index is for pubkey cleared
        assert!(accounts
            .accounts_index
            .read()
            .unwrap()
            .account_maps
            .get(&pubkey)
            .is_none());

        // slot 1 & 2 should not have any stores
        assert_no_stores(&accounts, 1);
        assert_no_stores(&accounts, 2);
    }

    #[test]
    fn test_accounts_db_serialize_zero_and_free() {
        solana_logger::setup();

        let some_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);
        let pubkey = Pubkey::new_rand();
        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let account2 = Account::new(some_lamport + 1, no_data, &owner);
        let pubkey2 = Pubkey::new_rand();

        let filler_account = Account::new(some_lamport, no_data, &owner);
        let filler_account_pubkey = Pubkey::new_rand();

        let accounts = AccountsDB::new_single();

        let mut current_slot = 1;
        accounts.store(current_slot, &[(&pubkey, &account)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&pubkey, &zero_lamport_account)]);
        accounts.store(current_slot, &[(&pubkey2, &account2)]);

        // Store enough accounts such that an additional store for slot 2 is created.
        while accounts
            .storage
            .read()
            .unwrap()
            .0
            .get(&current_slot)
            .unwrap()
            .len()
            < 2
        {
            accounts.store(current_slot, &[(&filler_account_pubkey, &filler_account)]);
        }
        accounts.add_root(current_slot);

        assert_load_account(&accounts, current_slot, pubkey, zero_lamport);

        accounts.print_accounts_stats("accounts");

        accounts.clean_accounts(None);

        accounts.print_accounts_stats("accounts_post_purge");
        let accounts = reconstruct_accounts_db_via_serialization(&accounts, current_slot);

        accounts.print_accounts_stats("reconstructed");

        assert_load_account(&accounts, current_slot, pubkey, zero_lamport);
    }

    fn with_chained_zero_lamport_accounts<F>(f: F)
    where
        F: Fn(AccountsDB, Slot) -> AccountsDB,
    {
        let some_lamport = 223;
        let zero_lamport = 0;
        let dummy_lamport = 999;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);
        let account2 = Account::new(some_lamport + 100_001, no_data, &owner);
        let account3 = Account::new(some_lamport + 100_002, no_data, &owner);
        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let pubkey = Pubkey::new_rand();
        let purged_pubkey1 = Pubkey::new_rand();
        let purged_pubkey2 = Pubkey::new_rand();

        let dummy_account = Account::new(dummy_lamport, no_data, &owner);
        let dummy_pubkey = Pubkey::default();

        let accounts = AccountsDB::new_single();

        let mut current_slot = 1;
        accounts.store(current_slot, &[(&pubkey, &account)]);
        accounts.store(current_slot, &[(&purged_pubkey1, &account2)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey1, &zero_lamport_account)]);
        accounts.store(current_slot, &[(&purged_pubkey2, &account3)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey2, &zero_lamport_account)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&dummy_pubkey, &dummy_account)]);
        accounts.add_root(current_slot);

        accounts.print_accounts_stats("pre_f");
        accounts.update_accounts_hash(4, &HashMap::default());

        let accounts = f(accounts, current_slot);

        accounts.print_accounts_stats("post_f");

        assert_load_account(&accounts, current_slot, pubkey, some_lamport);
        assert_load_account(&accounts, current_slot, purged_pubkey1, 0);
        assert_load_account(&accounts, current_slot, purged_pubkey2, 0);
        assert_load_account(&accounts, current_slot, dummy_pubkey, dummy_lamport);

        accounts
            .verify_bank_hash_and_lamports(4, &HashMap::default(), 1222)
            .unwrap();
    }

    #[test]
    fn test_accounts_purge_chained_purge_before_snapshot_restore() {
        solana_logger::setup();
        with_chained_zero_lamport_accounts(|accounts, current_slot| {
            accounts.clean_accounts(None);
            reconstruct_accounts_db_via_serialization(&accounts, current_slot)
        });
    }

    #[test]
    fn test_accounts_purge_chained_purge_after_snapshot_restore() {
        solana_logger::setup();
        with_chained_zero_lamport_accounts(|accounts, current_slot| {
            let accounts = reconstruct_accounts_db_via_serialization(&accounts, current_slot);
            accounts.print_accounts_stats("after_reconstruct");
            accounts.clean_accounts(None);
            reconstruct_accounts_db_via_serialization(&accounts, current_slot)
        });
    }

    #[test]
    #[ignore]
    fn test_store_account_stress() {
        let slot = 42;
        let num_threads = 2;

        let min_file_bytes = std::mem::size_of::<StoredMeta>()
            + std::mem::size_of::<crate::append_vec::AccountMeta>();

        let db = Arc::new(AccountsDB::new_sized(Vec::new(), min_file_bytes as u64));

        db.add_root(slot);
        let thread_hdls: Vec<_> = (0..num_threads)
            .map(|_| {
                let db = db.clone();
                std::thread::Builder::new()
                    .name("account-writers".to_string())
                    .spawn(move || {
                        let pubkey = Pubkey::new_rand();
                        let mut account = Account::new(1, 0, &pubkey);
                        let mut i = 0;
                        loop {
                            let account_bal = thread_rng().gen_range(1, 99);
                            account.lamports = account_bal;
                            db.store(slot, &[(&pubkey, &account)]);

                            let (account, slot) =
                                db.load_slow(&HashMap::new(), &pubkey).unwrap_or_else(|| {
                                    panic!("Could not fetch stored account {}, iter {}", pubkey, i)
                                });
                            assert_eq!(slot, slot);
                            assert_eq!(account.lamports, account_bal);
                            i += 1;
                        }
                    })
                    .unwrap()
            })
            .collect();

        for t in thread_hdls {
            t.join().unwrap();
        }
    }

    #[test]
    fn test_accountsdb_scan_accounts() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let key = Pubkey::default();
        let key0 = Pubkey::new_rand();
        let account0 = Account::new(1, 0, &key);

        db.store(0, &[(&key0, &account0)]);

        let key1 = Pubkey::new_rand();
        let account1 = Account::new(2, 0, &key);
        db.store(1, &[(&key1, &account1)]);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let accounts: Vec<Account> =
            db.scan_accounts(&ancestors, |accounts: &mut Vec<Account>, option| {
                if let Some(data) = option {
                    accounts.push(data.1);
                }
            });
        assert_eq!(accounts, vec![account0]);

        let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
        let accounts: Vec<Account> =
            db.scan_accounts(&ancestors, |accounts: &mut Vec<Account>, option| {
                if let Some(data) = option {
                    accounts.push(data.1);
                }
            });
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn test_cleanup_key_not_removed() {
        solana_logger::setup();
        let db = AccountsDB::new_single();

        let key = Pubkey::default();
        let key0 = Pubkey::new_rand();
        let account0 = Account::new(1, 0, &key);

        db.store(0, &[(&key0, &account0)]);

        let key1 = Pubkey::new_rand();
        let account1 = Account::new(2, 0, &key);
        db.store(1, &[(&key1, &account1)]);

        db.print_accounts_stats("pre");

        let slots: HashSet<Slot> = HashSet::from_iter(vec![1].into_iter());
        let purge_keys = vec![(key1, slots)];
        let (_reclaims, dead_keys) = db.purge_keys_exact(purge_keys);

        let account2 = Account::new(3, 0, &key);
        db.store(2, &[(&key1, &account2)]);

        db.handle_dead_keys(dead_keys);

        db.print_accounts_stats("post");
        let ancestors = vec![(2, 0)].into_iter().collect();
        assert_eq!(db.load_slow(&ancestors, &key1).unwrap().0.lamports, 3);
    }

    #[test]
    fn test_store_large_account() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let data_len = DEFAULT_FILE_SIZE as usize + 7;
        let account = Account::new(1, data_len, &key);

        db.store(0, &[(&key, &account)]);

        let ancestors = vec![(0, 0)].into_iter().collect();
        let ret = db.load_slow(&ancestors, &key).unwrap();
        assert_eq!(ret.0.data.len(), data_len);
    }

    pub fn copy_append_vecs<P: AsRef<Path>>(
        accounts_db: &AccountsDB,
        output_dir: P,
    ) -> IOResult<()> {
        let storage_entries = accounts_db.get_snapshot_storages(Slot::max_value());
        for storage in storage_entries.iter().flatten() {
            let storage_path = storage.get_path();
            let output_path = output_dir.as_ref().join(
                storage_path
                    .file_name()
                    .expect("Invalid AppendVec file path"),
            );

            fs::copy(storage_path, output_path)?;
        }

        Ok(())
    }

    #[test]
    fn test_hash_frozen_account_data() {
        let account = Account::new(1, 42, &Pubkey::default());

        let hash = AccountsDB::hash_frozen_account_data(&account);
        assert_ne!(hash, Hash::default()); // Better not be the default Hash

        // Lamports changes to not affect the hash
        let mut account_modified = account.clone();
        account_modified.lamports -= 1;
        assert_eq!(
            hash,
            AccountsDB::hash_frozen_account_data(&account_modified)
        );

        // Rent epoch may changes to not affect the hash
        let mut account_modified = account.clone();
        account_modified.rent_epoch += 1;
        assert_eq!(
            hash,
            AccountsDB::hash_frozen_account_data(&account_modified)
        );

        // Account data may not be modified
        let mut account_modified = account.clone();
        account_modified.data[0] = 42;
        assert_ne!(
            hash,
            AccountsDB::hash_frozen_account_data(&account_modified)
        );

        // Owner may not be modified
        let mut account_modified = account.clone();
        account_modified.owner =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        assert_ne!(
            hash,
            AccountsDB::hash_frozen_account_data(&account_modified)
        );

        // Executable may not be modified
        let mut account_modified = account;
        account_modified.executable = true;
        assert_ne!(
            hash,
            AccountsDB::hash_frozen_account_data(&account_modified)
        );
    }

    #[test]
    fn test_frozen_account_lamport_increase() {
        let frozen_pubkey =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let mut account = Account::new(1, 42, &frozen_pubkey);
        db.store(0, &[(&frozen_pubkey, &account)]);

        let ancestors = vec![(0, 0)].into_iter().collect();
        db.freeze_accounts(&ancestors, &[frozen_pubkey]);

        // Store with no account changes is ok
        db.store(0, &[(&frozen_pubkey, &account)]);

        // Store with an increase in lamports is ok
        account.lamports = 2;
        db.store(0, &[(&frozen_pubkey, &account)]);

        // Store with an decrease that does not go below the frozen amount of lamports is tolerated
        account.lamports = 1;
        db.store(0, &[(&frozen_pubkey, &account)]);

        // A store of any value over the frozen value of '1' across different slots is also ok
        account.lamports = 3;
        db.store(1, &[(&frozen_pubkey, &account)]);
        account.lamports = 2;
        db.store(2, &[(&frozen_pubkey, &account)]);
        account.lamports = 1;
        db.store(3, &[(&frozen_pubkey, &account)]);
    }

    #[test]
    #[should_panic(
        expected = "Frozen account My11111111111111111111111111111111111111111 modified.  Lamports decreased from 1 to 0"
    )]
    fn test_frozen_account_lamport_decrease() {
        let frozen_pubkey =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let mut account = Account::new(1, 42, &frozen_pubkey);
        db.store(0, &[(&frozen_pubkey, &account)]);

        let ancestors = vec![(0, 0)].into_iter().collect();
        db.freeze_accounts(&ancestors, &[frozen_pubkey]);

        // Store with a decrease below the frozen amount of lamports is not ok
        account.lamports -= 1;
        db.store(0, &[(&frozen_pubkey, &account)]);
    }

    #[test]
    #[should_panic(
        expected = "Unable to freeze an account that does not exist: My11111111111111111111111111111111111111111"
    )]
    fn test_frozen_account_nonexistent() {
        let frozen_pubkey =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let ancestors = vec![(0, 0)].into_iter().collect();
        db.freeze_accounts(&ancestors, &[frozen_pubkey]);
    }

    #[test]
    #[should_panic(
        expected = "Frozen account My11111111111111111111111111111111111111111 modified.  Hash changed from 8wHcxDkjiwdrkPAsDnmNrF1UDGJFAtZzPQBSVweY3yRA to JdscGYB1uczVssmYuJusDD1Bfe6wpNeeho8XjcH8inN"
    )]
    fn test_frozen_account_data_modified() {
        let frozen_pubkey =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let mut account = Account::new(1, 42, &frozen_pubkey);
        db.store(0, &[(&frozen_pubkey, &account)]);

        let ancestors = vec![(0, 0)].into_iter().collect();
        db.freeze_accounts(&ancestors, &[frozen_pubkey]);

        account.data[0] = 42;
        db.store(0, &[(&frozen_pubkey, &account)]);
    }

    #[test]
    fn test_hash_stored_account() {
        // This test uses some UNSAFE trick to detect most of account's field
        // addition and deletion without changing the hash code

        const ACCOUNT_DATA_LEN: usize = 3;
        // the type of InputTuple elements must not contain references;
        // they should be simple scalars or data blobs
        type InputTuple = (
            Slot,
            StoredMeta,
            AccountMeta,
            [u8; ACCOUNT_DATA_LEN],
            usize, // for StoredAccount::offset
            Hash,
        );
        const INPUT_LEN: usize = std::mem::size_of::<InputTuple>();
        type InputBlob = [u8; INPUT_LEN];
        let mut blob: InputBlob = [0u8; INPUT_LEN];

        // spray memory with decreasing counts so that, data layout can be detected.
        for (i, byte) in blob.iter_mut().enumerate() {
            *byte = (INPUT_LEN - i) as u8;
        }

        //UNSAFE: forcibly cast the special byte pattern to actual account fields.
        let (slot, meta, account_meta, data, offset, hash): InputTuple =
            unsafe { std::mem::transmute::<InputBlob, InputTuple>(blob) };

        let stored_account = StoredAccount {
            meta: &meta,
            account_meta: &account_meta,
            data: &data,
            offset,
            hash: &hash,
        };
        let account = stored_account.clone_account();
        let expected_account_hash =
            Hash::from_str("4StuvYHFd7xuShVXB94uHHvpqGMCaacdZnYB74QQkPA1").unwrap();

        assert_eq!(
            AccountsDB::hash_stored_account(slot, &stored_account, &ClusterType::Development),
            expected_account_hash,
            "StoredAccount's data layout might be changed; update hashing if needed."
        );
        assert_eq!(
            AccountsDB::hash_account(
                slot,
                &account,
                &stored_account.meta.pubkey,
                &ClusterType::Development
            ),
            expected_account_hash,
            "Account-based hashing must be consistent with StoredAccount-based one."
        );
    }

    #[test]
    fn test_bank_hash_stats() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let some_data_len = 5;
        let some_slot: Slot = 0;
        let account = Account::new(1, some_data_len, &key);
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        db.store(some_slot, &[(&key, &account)]);
        let mut account = db.load_slow(&ancestors, &key).unwrap().0;
        account.lamports -= 1;
        account.executable = true;
        db.store(some_slot, &[(&key, &account)]);
        db.add_root(some_slot);

        let bank_hashes = db.bank_hashes.read().unwrap();
        let bank_hash = bank_hashes.get(&some_slot).unwrap();
        assert_eq!(bank_hash.stats.num_updated_accounts, 1);
        assert_eq!(bank_hash.stats.num_removed_accounts, 1);
        assert_eq!(bank_hash.stats.num_lamports_stored, 1);
        assert_eq!(bank_hash.stats.total_data_len, 2 * some_data_len as u64);
        assert_eq!(bank_hash.stats.num_executable_accounts, 1);
    }

    #[test]
    fn test_verify_bank_hash() {
        use BankHashVerificationError::*;
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::new_rand();
        let some_data_len = 0;
        let some_slot: Slot = 0;
        let account = Account::new(1, some_data_len, &key);
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        db.store(some_slot, &[(&key, &account)]);
        db.add_root(some_slot);
        db.update_accounts_hash(some_slot, &ancestors);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Ok(_)
        );

        db.bank_hashes.write().unwrap().remove(&some_slot).unwrap();
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Err(MissingBankHash)
        );

        let some_bank_hash = Hash::new(&[0xca; HASH_BYTES]);
        let bank_hash_info = BankHashInfo {
            hash: some_bank_hash,
            snapshot_hash: Hash::new(&[0xca; HASH_BYTES]),
            stats: BankHashStats::default(),
        };
        db.bank_hashes
            .write()
            .unwrap()
            .insert(some_slot, bank_hash_info);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Err(MismatchedBankHash)
        );
    }

    #[test]
    fn test_verify_bank_capitalization() {
        use BankHashVerificationError::*;
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::new_rand();
        let some_data_len = 0;
        let some_slot: Slot = 0;
        let account = Account::new(1, some_data_len, &key);
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        db.store(some_slot, &[(&key, &account)]);
        db.add_root(some_slot);
        db.update_accounts_hash(some_slot, &ancestors);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Ok(_)
        );

        let native_account_pubkey = Pubkey::new_rand();
        db.store(
            some_slot,
            &[(
                &native_account_pubkey,
                &solana_sdk::native_loader::create_loadable_account("foo"),
            )],
        );
        db.update_accounts_hash(some_slot, &ancestors);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Ok(_)
        );

        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 10),
            Err(MismatchedTotalLamports(expected, actual)) if expected == 1 && actual == 10
        );
    }

    #[test]
    fn test_verify_bank_hash_no_account() {
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let some_slot: Slot = 0;
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        db.bank_hashes
            .write()
            .unwrap()
            .insert(some_slot, BankHashInfo::default());
        db.add_root(some_slot);
        db.update_accounts_hash(some_slot, &ancestors);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 0),
            Ok(_)
        );
    }

    #[test]
    fn test_verify_bank_hash_bad_account_hash() {
        use BankHashVerificationError::*;
        solana_logger::setup();
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let some_data_len = 0;
        let some_slot: Slot = 0;
        let account = Account::new(1, some_data_len, &key);
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        let accounts = &[(&key, &account)];
        // update AccountsDB's bank hash but discard real account hashes
        db.hash_accounts(some_slot, accounts, &ClusterType::Development);
        // provide bogus account hashes
        let some_hash = Hash::new(&[0xca; HASH_BYTES]);
        db.store_with_hashes(some_slot, accounts, &[some_hash]);
        db.add_root(some_slot);
        assert_matches!(
            db.verify_bank_hash_and_lamports(some_slot, &ancestors, 1),
            Err(MismatchedAccountHash)
        );
    }

    #[test]
    fn test_bad_bank_hash() {
        use solana_sdk::signature::{Keypair, Signer};
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let some_slot: Slot = 0;
        let ancestors: Ancestors = [(some_slot, 0)].iter().copied().collect();

        for _ in 0..10_000 {
            let num_accounts = thread_rng().gen_range(0, 100);
            let accounts_keys: Vec<_> = (0..num_accounts)
                .map(|_| {
                    let key = Keypair::new().pubkey();
                    let lamports = thread_rng().gen_range(0, 100);
                    let some_data_len = thread_rng().gen_range(0, 1000);
                    let account = Account::new(lamports, some_data_len, &key);
                    (key, account)
                })
                .collect();
            let account_refs: Vec<_> = accounts_keys
                .iter()
                .map(|(key, account)| (key, account))
                .collect();
            db.store(some_slot, &account_refs);

            for (key, account) in &accounts_keys {
                assert_eq!(
                    db.load_account_hash(&ancestors, key),
                    AccountsDB::hash_account(some_slot, &account, &key, &ClusterType::Development)
                );
            }
        }
    }

    #[test]
    fn test_get_snapshot_storages_empty() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);
        assert!(db.get_snapshot_storages(0).is_empty());
    }

    #[test]
    fn test_get_snapshot_storages_only_older_than_or_equal_to_snapshot_slot() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let account = Account::new(1, 0, &key);
        let before_slot = 0;
        let base_slot = before_slot + 1;
        let after_slot = base_slot + 1;

        db.add_root(base_slot);
        db.store(base_slot, &[(&key, &account)]);
        assert!(db.get_snapshot_storages(before_slot).is_empty());

        assert_eq!(1, db.get_snapshot_storages(base_slot).len());
        assert_eq!(1, db.get_snapshot_storages(after_slot).len());
    }

    #[test]
    fn test_get_snapshot_storages_only_non_empty() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let account = Account::new(1, 0, &key);
        let base_slot = 0;
        let after_slot = base_slot + 1;

        db.store(base_slot, &[(&key, &account)]);
        db.storage
            .write()
            .unwrap()
            .0
            .get_mut(&base_slot)
            .unwrap()
            .clear();
        db.add_root(base_slot);
        assert!(db.get_snapshot_storages(after_slot).is_empty());

        db.store(base_slot, &[(&key, &account)]);
        assert_eq!(1, db.get_snapshot_storages(after_slot).len());
    }

    #[test]
    fn test_get_snapshot_storages_only_roots() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let account = Account::new(1, 0, &key);
        let base_slot = 0;
        let after_slot = base_slot + 1;

        db.store(base_slot, &[(&key, &account)]);
        assert!(db.get_snapshot_storages(after_slot).is_empty());

        db.add_root(base_slot);
        assert_eq!(1, db.get_snapshot_storages(after_slot).len());
    }

    #[test]
    fn test_get_snapshot_storages_exclude_empty() {
        let db = AccountsDB::new(Vec::new(), &ClusterType::Development);

        let key = Pubkey::default();
        let account = Account::new(1, 0, &key);
        let base_slot = 0;
        let after_slot = base_slot + 1;

        db.store(base_slot, &[(&key, &account)]);
        db.add_root(base_slot);
        assert_eq!(1, db.get_snapshot_storages(after_slot).len());

        let storage = db.storage.read().unwrap();
        storage.0[&0].values().next().unwrap().remove_account();
        assert!(db.get_snapshot_storages(after_slot).is_empty());
    }

    #[test]
    #[should_panic(expected = "double remove of account in slot: 0/store: 0!!")]
    fn test_storage_remove_account_double_remove() {
        let accounts = AccountsDB::new(Vec::new(), &ClusterType::Development);
        let pubkey = Pubkey::new_rand();
        let account = Account::new(1, 0, &Account::default().owner);
        accounts.store(0, &[(&pubkey, &account)]);
        let storage = accounts.storage.read().unwrap();
        let storage_entry = storage.0[&0].values().next().unwrap();
        storage_entry.remove_account();
        storage_entry.remove_account();
    }

    #[test]
    fn test_accounts_purge_long_chained_after_snapshot_restore() {
        solana_logger::setup();
        let old_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(old_lamport, no_data, &owner);
        let account2 = Account::new(old_lamport + 100_001, no_data, &owner);
        let account3 = Account::new(old_lamport + 100_002, no_data, &owner);
        let dummy_account = Account::new(99_999_999, no_data, &owner);
        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let pubkey = Pubkey::new_rand();
        let dummy_pubkey = Pubkey::new_rand();
        let purged_pubkey1 = Pubkey::new_rand();
        let purged_pubkey2 = Pubkey::new_rand();

        let mut current_slot = 0;
        let accounts = AccountsDB::new_single();

        // create intermediate updates to purged_pubkey1 so that
        // generate_index must add slots as root last at once
        current_slot += 1;
        accounts.store(current_slot, &[(&pubkey, &account)]);
        accounts.store(current_slot, &[(&purged_pubkey1, &account2)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey1, &account2)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey1, &account2)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey1, &zero_lamport_account)]);
        accounts.store(current_slot, &[(&purged_pubkey2, &account3)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&purged_pubkey2, &zero_lamport_account)]);
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store(current_slot, &[(&dummy_pubkey, &dummy_account)]);
        accounts.add_root(current_slot);

        accounts.print_count_and_status("before reconstruct");
        let accounts = reconstruct_accounts_db_via_serialization(&accounts, current_slot);
        accounts.print_count_and_status("before purge zero");
        accounts.clean_accounts(None);
        accounts.print_count_and_status("after purge zero");

        assert_load_account(&accounts, current_slot, pubkey, old_lamport);
        assert_load_account(&accounts, current_slot, purged_pubkey1, 0);
        assert_load_account(&accounts, current_slot, purged_pubkey2, 0);
    }

    #[test]
    fn test_accounts_clean_after_snapshot_restore_then_old_revives() {
        solana_logger::setup();
        let old_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let dummy_lamport = 999_999;
        let owner = Account::default().owner;

        let account = Account::new(old_lamport, no_data, &owner);
        let account2 = Account::new(old_lamport + 100_001, no_data, &owner);
        let account3 = Account::new(old_lamport + 100_002, no_data, &owner);
        let dummy_account = Account::new(dummy_lamport, no_data, &owner);
        let zero_lamport_account = Account::new(zero_lamport, no_data, &owner);

        let pubkey1 = Pubkey::new_rand();
        let pubkey2 = Pubkey::new_rand();
        let dummy_pubkey = Pubkey::new_rand();

        let mut current_slot = 0;
        let accounts = AccountsDB::new_single();

        // A: Initialize AccountsDB with pubkey1 and pubkey2
        current_slot += 1;
        accounts.store(current_slot, &[(&pubkey1, &account)]);
        accounts.store(current_slot, &[(&pubkey2, &account)]);
        accounts.add_root(current_slot);

        // B: Test multiple updates to pubkey1 in a single slot/storage
        current_slot += 1;
        assert_eq!(0, accounts.alive_account_count_in_store(current_slot));
        assert_eq!(1, accounts.ref_count_for_pubkey(&pubkey1));
        accounts.store(current_slot, &[(&pubkey1, &account2)]);
        accounts.store(current_slot, &[(&pubkey1, &account2)]);
        assert_eq!(1, accounts.alive_account_count_in_store(current_slot));
        // Stores to same pubkey, same slot only count once towards the
        // ref count
        assert_eq!(2, accounts.ref_count_for_pubkey(&pubkey1));
        accounts.add_root(current_slot);

        // C: Yet more update to trigger lazy clean of step A
        current_slot += 1;
        assert_eq!(2, accounts.ref_count_for_pubkey(&pubkey1));
        accounts.store(current_slot, &[(&pubkey1, &account3)]);
        assert_eq!(3, accounts.ref_count_for_pubkey(&pubkey1));
        accounts.add_root(current_slot);

        // D: Make pubkey1 0-lamport; also triggers clean of step B
        current_slot += 1;
        assert_eq!(3, accounts.ref_count_for_pubkey(&pubkey1));
        accounts.store(current_slot, &[(&pubkey1, &zero_lamport_account)]);
        accounts.clean_accounts(None);

        assert_eq!(
            // Removed one reference from the dead slot (reference only counted once
            // even though there were two stores to the pubkey in that slot)
            3, /* == 3 - 1 + 1 */
            accounts.ref_count_for_pubkey(&pubkey1)
        );
        accounts.add_root(current_slot);

        // E: Avoid missing bank hash error
        current_slot += 1;
        accounts.store(current_slot, &[(&dummy_pubkey, &dummy_account)]);
        accounts.add_root(current_slot);

        assert_load_account(&accounts, current_slot, pubkey1, zero_lamport);
        assert_load_account(&accounts, current_slot, pubkey2, old_lamport);
        assert_load_account(&accounts, current_slot, dummy_pubkey, dummy_lamport);

        // At this point, there is no index entries for A and B
        // If step C and step D should be purged, snapshot restore would cause
        // pubkey1 to be revived as the state of step A.
        // So, prevent that from happening by introducing refcount
        accounts.clean_accounts(None);
        let accounts = reconstruct_accounts_db_via_serialization(&accounts, current_slot);
        accounts.clean_accounts(None);

        assert_load_account(&accounts, current_slot, pubkey1, zero_lamport);
        assert_load_account(&accounts, current_slot, pubkey2, old_lamport);
        assert_load_account(&accounts, current_slot, dummy_pubkey, dummy_lamport);

        // F: Finally, make Step A cleanable
        current_slot += 1;
        accounts.store(current_slot, &[(&pubkey2, &account)]);
        accounts.add_root(current_slot);

        // Do clean
        accounts.clean_accounts(None);

        // Ensure pubkey2 is cleaned from the index finally
        assert_not_load_account(&accounts, current_slot, pubkey1);
        assert_load_account(&accounts, current_slot, pubkey2, old_lamport);
        assert_load_account(&accounts, current_slot, dummy_pubkey, dummy_lamport);
    }

    #[test]
    fn test_clean_dead_slots_empty() {
        let accounts = AccountsDB::new_single();
        let mut dead_slots = HashSet::new();
        dead_slots.insert(10);
        accounts.clean_dead_slots(&dead_slots);
    }

    #[test]
    fn test_shrink_all_slots_none() {
        let accounts = AccountsDB::new_single();

        for _ in 0..10 {
            assert_eq!(0, accounts.process_stale_slot());
        }

        accounts.shrink_all_slots();
    }

    #[test]
    fn test_shrink_next_slots() {
        let accounts = AccountsDB::new_single();

        let mut current_slot = 7;

        assert_eq!(
            vec![None, None, None],
            (0..3)
                .map(|_| accounts.next_shrink_slot())
                .collect::<Vec<_>>()
        );

        accounts.add_root(current_slot);

        assert_eq!(
            vec![Some(7), Some(7), Some(7)],
            (0..3)
                .map(|_| accounts.next_shrink_slot())
                .collect::<Vec<_>>()
        );

        current_slot += 1;
        accounts.add_root(current_slot);

        let slots = (0..6)
            .map(|_| accounts.next_shrink_slot())
            .collect::<Vec<_>>();

        // Because the origin of this data is HashMap (not BTreeMap), key order is arbitrary per cycle.
        assert!(
            vec![Some(7), Some(8), Some(7), Some(8), Some(7), Some(8)] == slots
                || vec![Some(8), Some(7), Some(8), Some(7), Some(8), Some(7)] == slots
        );
    }

    #[test]
    fn test_shrink_reset_uncleaned_roots() {
        let accounts = AccountsDB::new_single();

        accounts.reset_uncleaned_roots();
        assert_eq!(
            *accounts.shrink_candidate_slots.lock().unwrap(),
            vec![] as Vec<Slot>
        );

        accounts.add_root(0);
        accounts.add_root(1);
        accounts.add_root(2);

        accounts.reset_uncleaned_roots();
        let actual_slots = accounts.shrink_candidate_slots.lock().unwrap().clone();
        assert_eq!(actual_slots, vec![] as Vec<Slot>);

        accounts.reset_uncleaned_roots();
        let mut actual_slots = accounts.shrink_candidate_slots.lock().unwrap().clone();
        actual_slots.sort();
        assert_eq!(actual_slots, vec![0, 1, 2]);

        accounts.accounts_index.write().unwrap().roots.clear();
        let mut actual_slots = (0..5)
            .map(|_| accounts.next_shrink_slot())
            .collect::<Vec<_>>();
        actual_slots.sort();
        assert_eq!(actual_slots, vec![None, None, Some(0), Some(1), Some(2)],);
    }

    #[test]
    fn test_shrink_stale_slots_processed() {
        solana_logger::setup();

        let accounts = AccountsDB::new_single();

        let pubkey_count = 100;
        let pubkeys: Vec<_> = (0..pubkey_count).map(|_| Pubkey::new_rand()).collect();

        let some_lamport = 223;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);

        let mut current_slot = 0;

        current_slot += 1;
        for pubkey in &pubkeys {
            accounts.store(current_slot, &[(&pubkey, &account)]);
        }
        let shrink_slot = current_slot;
        accounts.add_root(current_slot);

        current_slot += 1;
        let pubkey_count_after_shrink = 10;
        let updated_pubkeys = &pubkeys[0..pubkey_count - pubkey_count_after_shrink];

        for pubkey in updated_pubkeys {
            accounts.store(current_slot, &[(&pubkey, &account)]);
        }
        accounts.add_root(current_slot);

        accounts.clean_accounts(None);

        assert_eq!(
            pubkey_count,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );
        accounts.shrink_all_slots();
        assert_eq!(
            pubkey_count_after_shrink,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );

        let no_ancestors = HashMap::default();
        accounts.update_accounts_hash(current_slot, &no_ancestors);
        accounts
            .verify_bank_hash_and_lamports(current_slot, &no_ancestors, 22300)
            .unwrap();

        let accounts = reconstruct_accounts_db_via_serialization(&accounts, current_slot);
        accounts
            .verify_bank_hash_and_lamports(current_slot, &no_ancestors, 22300)
            .unwrap();

        // repeating should be no-op
        accounts.shrink_all_slots();
        assert_eq!(
            pubkey_count_after_shrink,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );
    }

    #[test]
    fn test_shrink_stale_slots_skipped() {
        solana_logger::setup();

        let accounts = AccountsDB::new_single();

        let pubkey_count = 100;
        let pubkeys: Vec<_> = (0..pubkey_count).map(|_| Pubkey::new_rand()).collect();

        let some_lamport = 223;
        let no_data = 0;
        let owner = Account::default().owner;

        let account = Account::new(some_lamport, no_data, &owner);

        let mut current_slot = 0;

        current_slot += 1;
        for pubkey in &pubkeys {
            accounts.store(current_slot, &[(&pubkey, &account)]);
        }
        let shrink_slot = current_slot;
        accounts.add_root(current_slot);

        current_slot += 1;
        let pubkey_count_after_shrink = 90;
        let updated_pubkeys = &pubkeys[0..pubkey_count - pubkey_count_after_shrink];

        for pubkey in updated_pubkeys {
            accounts.store(current_slot, &[(&pubkey, &account)]);
        }
        accounts.add_root(current_slot);

        accounts.clean_accounts(None);

        assert_eq!(
            pubkey_count,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );

        // Only, try to shrink stale slots.
        accounts.shrink_all_stale_slots();
        assert_eq!(
            pubkey_count,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );

        // Now, do full-shrink.
        accounts.shrink_all_slots();
        assert_eq!(
            pubkey_count_after_shrink,
            accounts.all_account_count_in_append_vec(shrink_slot)
        );
    }

    #[test]
    fn test_delete_dependencies() {
        solana_logger::setup();
        let mut accounts_index = AccountsIndex::default();
        let key0 = Pubkey::new_from_array([0u8; 32]);
        let key1 = Pubkey::new_from_array([1u8; 32]);
        let key2 = Pubkey::new_from_array([2u8; 32]);
        let info0 = AccountInfo {
            store_id: 0,
            offset: 0,
            lamports: 0,
        };
        let info1 = AccountInfo {
            store_id: 1,
            offset: 0,
            lamports: 0,
        };
        let info2 = AccountInfo {
            store_id: 2,
            offset: 0,
            lamports: 0,
        };
        let info3 = AccountInfo {
            store_id: 3,
            offset: 0,
            lamports: 0,
        };
        let mut reclaims = vec![];
        accounts_index.insert(0, &key0, info0, &mut reclaims);
        accounts_index.insert(1, &key0, info1.clone(), &mut reclaims);
        accounts_index.insert(1, &key1, info1, &mut reclaims);
        accounts_index.insert(2, &key1, info2.clone(), &mut reclaims);
        accounts_index.insert(2, &key2, info2, &mut reclaims);
        accounts_index.insert(3, &key2, info3, &mut reclaims);
        accounts_index.add_root(0);
        accounts_index.add_root(1);
        accounts_index.add_root(2);
        accounts_index.add_root(3);
        let mut purges = HashMap::new();
        purges.insert(key0, accounts_index.would_purge(&key0));
        purges.insert(key1, accounts_index.would_purge(&key1));
        purges.insert(key2, accounts_index.would_purge(&key2));
        for (key, (list, ref_count)) in &purges {
            info!(" purge {} ref_count {} =>", key, ref_count);
            for x in list {
                info!("  {:?}", x);
            }
        }

        let mut store_counts = HashMap::new();
        store_counts.insert(0, (0, HashSet::from_iter(vec![key0])));
        store_counts.insert(1, (0, HashSet::from_iter(vec![key0, key1])));
        store_counts.insert(2, (0, HashSet::from_iter(vec![key1, key2])));
        store_counts.insert(3, (1, HashSet::from_iter(vec![key2])));
        AccountsDB::calc_delete_dependencies(&purges, &mut store_counts);
        let mut stores: Vec<_> = store_counts.keys().cloned().collect();
        stores.sort();
        for store in &stores {
            info!(
                "store: {:?} : {:?}",
                store,
                store_counts.get(&store).unwrap()
            );
        }
        for x in 0..3 {
            assert!(store_counts[&x].0 >= 1);
        }
    }

    #[test]
    fn test_shrink_and_clean() {
        solana_logger::setup();

        // repeat the whole test scenario
        for _ in 0..5 {
            let accounts = Arc::new(AccountsDB::new_single());
            let accounts_for_shrink = accounts.clone();

            // spawn the slot shrinking background thread
            let exit = Arc::new(AtomicBool::default());
            let exit_for_shrink = exit.clone();
            let shrink_thread = std::thread::spawn(move || loop {
                if exit_for_shrink.load(Ordering::Relaxed) {
                    break;
                }
                accounts_for_shrink.process_stale_slot();
            });

            let mut alive_accounts = vec![];
            let owner = Pubkey::default();

            // populate the AccountsDB with plenty of food for slot shrinking
            // also this simulates realistic some heavy spike account updates in the wild
            for current_slot in 0..1000 {
                while alive_accounts.len() <= 10 {
                    alive_accounts.push((
                        Pubkey::new_rand(),
                        Account::new(thread_rng().gen_range(0, 50), 0, &owner),
                    ));
                }

                alive_accounts.retain(|(_pubkey, account)| account.lamports >= 1);

                for (pubkey, account) in alive_accounts.iter_mut() {
                    account.lamports -= 1;
                    accounts.store(current_slot, &[(&pubkey, &account)]);
                }
                accounts.add_root(current_slot);
            }

            // let's dance.
            for _ in 0..10 {
                accounts.clean_accounts(None);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // cleanup
            exit.store(true, Ordering::Relaxed);
            shrink_thread.join().unwrap();
        }
    }

    #[test]
    fn test_account_balance_for_capitalization_normal() {
        // system accounts
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(10, &Pubkey::default(), false),
            10
        );
        // any random program data accounts
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(10, &Pubkey::new_rand(), false),
            10
        );
    }

    #[test]
    fn test_account_balance_for_capitalization_sysvar() {
        use solana_sdk::sysvar::Sysvar;

        let normal_sysvar = solana_sdk::slot_history::SlotHistory::default().create_account(1);
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(
                normal_sysvar.lamports,
                &normal_sysvar.owner,
                normal_sysvar.executable
            ),
            0
        );

        // currently transactions can send any lamports to sysvars although this is not sensible.
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(10, &solana_sdk::sysvar::id(), false),
            9
        );
    }

    #[test]
    fn test_account_balance_for_capitalization_native_program() {
        let normal_native_program = solana_sdk::native_loader::create_loadable_account("foo");
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(
                normal_native_program.lamports,
                &normal_native_program.owner,
                normal_native_program.executable
            ),
            0
        );

        // test maliciously assigned bogus native loader account
        assert_eq!(
            AccountsDB::account_balance_for_capitalization(
                1,
                &solana_sdk::native_loader::id(),
                false
            ),
            1
        )
    }

    #[test]
    fn test_checked_sum_for_capitalization_normal() {
        assert_eq!(
            AccountsDB::checked_sum_for_capitalization(vec![1, 2].into_iter()),
            3
        );
    }

    #[test]
    #[should_panic(expected = "overflow is detected while summing capitalization")]
    fn test_checked_sum_for_capitalization_overflow() {
        assert_eq!(
            AccountsDB::checked_sum_for_capitalization(vec![1, u64::max_value()].into_iter()),
            3
        );
    }
}
