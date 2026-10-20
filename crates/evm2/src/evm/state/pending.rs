//! Owned pending transaction state detached from the EVM.

use super::{
    Account, AccountChanges, AccountInfo, StateChangeSink, StateChangeSource, StorageOverlay,
    StorageSlot, Tracked,
};
use crate::{
    evm::{DbResult, DynDatabase},
    interpreter::Word,
};
use alloy_primitives::{
    Address,
    map::{AddressMap, AddressSet},
};

/// A transaction's loaded state, moved out of the EVM without committing it.
///
/// Produced by [`State::take_pending_state`](super::State::take_pending_state), or after
/// finalization by [`ExecutedTx::detach`](crate::ExecutedTx::detach), this is the transaction
/// overlay exactly as execution left it: every account and storage slot loaded during the
/// transaction, each carrying its transaction-boundary original value next to its present value.
/// Two consumers draw from it:
///
/// - [`Bal::commit`](crate::evm::Bal::commit) folds it into an EIP-7928 Block Access List,
///   recording loaded-but-unchanged entries as reads and changed ones as writes — the same fold the
///   EVM applies on transaction commit when its builder is enabled.
/// - [`StateChangeSource::visit`] streams it to a [`StateChangeSink`] in deterministic application
///   order: changed entries through the change callbacks (how persistence consumers, e.g. reth,
///   apply the transaction to the database) and loaded-but-unchanged entries through the read
///   callbacks.
///
/// A detached pending state can also be reattached to an EVM with
/// [`State::set_pending_state`](super::State::set_pending_state).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingState {
    /// Accounts loaded by the transaction: transaction-boundary original info, present info, and
    /// account-lifetime flags.
    pub(crate) accounts: AddressMap<Account>,
    /// Accounts selfdestructed by the transaction.
    pub(crate) selfdestructs: AddressSet,
}

impl PendingState {
    /// Returns whether the transaction loaded no accounts and no storage.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Returns the current account information when the account is present in pending state.
    #[inline]
    pub fn account_info(&self, address: &Address) -> Option<&AccountInfo> {
        self.accounts.get(address).and_then(|account| account.present.as_ref())
    }

    /// Inserts an account's transaction-boundary original and current values.
    pub fn insert_account(
        &mut self,
        address: Address,
        original: Option<AccountInfo>,
        current: Option<AccountInfo>,
    ) {
        let code_changed = original.as_ref().map(|account| account.code_hash)
            != current.as_ref().map(|account| account.code_hash);
        let account = self.accounts.entry(address).or_default();
        *account = Account {
            original,
            present: current,
            code_changed,
            storage: core::mem::take(&mut account.storage),
            ..Account::default()
        };
    }

    /// Inserts a storage slot's transaction-boundary original and current values.
    ///
    /// # Panics
    ///
    /// Panics if the owning account has not been inserted with [`Self::insert_account`].
    pub fn insert_storage(&mut self, address: Address, key: Word, original: Word, current: Word) {
        self.accounts
            .get_mut(&address)
            .expect("insert the account before its storage")
            .storage
            .slots
            .insert(
                key,
                StorageSlot {
                    value: Tracked::from_parts(original, current),
                    is_warm: false,
                    _non_exhaustive: (),
                },
            );
    }

    /// Returns whether the transaction contains any account or storage change.
    ///
    /// Loaded-but-unchanged accounts and storage slots are ignored.
    #[cfg(test)]
    pub(crate) fn is_changed(&self) -> bool {
        self.accounts.values().any(|account| account.is_changed() || account.storage.is_changed())
    }

    /// Returns each loaded account's changes, in an unspecified account order.
    #[inline]
    pub fn account_changes(&self) -> impl Iterator<Item = AccountChanges<'_>> {
        self.accounts.iter().map(|(&address, account)| {
            account.changes(address, self.selfdestructs.contains(&address))
        })
    }

    /// Copies a loaded account from another transaction layer without journaling it.
    ///
    /// Source account metadata and slots win, while target-only slots are retained unless the
    /// source wiped storage. Originals, warmth and lifecycle flags are copied with their values.
    /// An account the source did not load leaves the target unchanged.
    pub fn merge_account(&mut self, source: &Self, address: &Address) {
        let Some(mut account) = source.accounts.get(address).cloned() else { return };
        if !account.storage.wiped
            && let Some(target) = self.accounts.get(address)
        {
            for (key, slot) in &target.storage.slots {
                account.storage.slots.entry(*key).or_insert(*slot);
            }
        }
        self.accounts.insert(*address, account);
        if source.selfdestructs.contains(address) {
            self.selfdestructs.insert(*address);
        } else {
            self.selfdestructs.remove(address);
        }
    }

    /// Refreshes loaded values after an external transaction, preserving originals and lifecycle
    /// flags.
    ///
    /// The database must already include this overlay's writes and the external transaction.
    /// Reattaching the refreshed overlay requires a backing cache consistent with its retained
    /// originals, not the external transaction's current values. Stage the overlay first if
    /// database errors must not publish partial updates.
    pub fn refresh(&mut self, database: &mut impl DynDatabase) -> DbResult<()> {
        for (address, account) in &mut self.accounts {
            let mut current = database.get_account(address)?;
            if let Some(current) = &mut current
                && let Some(previous) = &account.present
                && current.code_hash == previous.code_hash
            {
                current.code = previous.code.clone();
            }
            account.code_changed = account.original.as_ref().map(|info| info.code_hash)
                != current.as_ref().map(|info| info.code_hash);
            if account.code_changed
                && let Some(current) = &mut current
            {
                current.code = Some(database.get_code_by_hash(&current.code_hash)?);
            }
            account.present = current;
            for (key, slot) in &mut account.storage.slots {
                slot.value.current = database.get_storage(address, key)?;
            }
            // The database already contains the wipe and any newly written slots.
            account.storage.wiped = false;
        }
        Ok(())
    }

    /// Returns a loaded account's storage for non-journaled updates to a detached overlay.
    pub fn storage_mut(&mut self, address: &Address) -> Option<&mut StorageOverlay> {
        self.accounts.get_mut(address).map(|account| &mut account.storage)
    }
}

impl StateChangeSource for PendingState {
    /// Visits the transaction's loaded entries account by account, in an unspecified account
    /// order. Each account's bytecode and storage precede its metadata.
    ///
    /// The same code hash may be visited more than once when several accounts share bytecode; sinks
    /// key bytecode by hash, so repeated visits are idempotent.
    ///
    /// Changed accounts — including created or selfdestructed accounts whose info ended up
    /// unchanged — go through [`StateChangeSink::account`]; loaded-but-unchanged entries go
    /// through the read callbacks.
    fn visit<S: StateChangeSink>(&self, sink: &mut S) -> Result<(), S::Error> {
        for changes in self.account_changes() {
            changes.visit(sink)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EvmFeatures, SpecId, Version,
        evm::{EmptyDB, State},
    };

    #[test]
    fn inserts_account_and_storage() {
        let address = Address::with_last_byte(0xaa);
        let key = Word::from(1);
        let original = AccountInfo { nonce: 1, ..AccountInfo::default() };
        let current = AccountInfo { nonce: 2, ..AccountInfo::default() };
        let mut state = PendingState::default();

        state.insert_account(address, Some(original.clone()), Some(current.clone()));
        state.insert_storage(address, key, Word::from(2), Word::from(3));

        assert_eq!(state.account_info(&address), Some(&current));
        assert_eq!(state.accounts[&address].original, Some(original));
        assert_eq!(state.accounts[&address].present, Some(current));
        assert_eq!(
            state.accounts[&address].storage.slots[&key].value,
            Tracked::from_parts(Word::from(2), Word::from(3))
        );
    }

    #[test]
    fn merging_loaded_accounts_preserves_slots_and_lifecycle() {
        let address = Address::with_last_byte(0xaa);
        let mut source = State::new(EmptyDB::default());
        source
            .create_account(&address, &address, &Word::ZERO, EvmFeatures::EIP161)
            .unwrap()
            .unwrap();
        source.storage_slot(&address, Word::ONE).unwrap().set(Word::from(23));
        let pending = source.take_pending_state();
        let mut target = State::new(EmptyDB::default());
        target.storage_slot(&address, Word::ONE).unwrap().set(Word::from(91));
        target.storage_slot(&address, Word::from(2)).unwrap().set(Word::from(47));
        target.account(&address).unwrap().mark_destructed();
        let mut merged = target.take_pending_state();
        merged.merge_account(&pending, &address);
        let mut resumed = State::new(EmptyDB::default());
        resumed.set_pending_state(merged);
        assert!(resumed.journal().is_empty());
        assert!(resumed.account(&address).unwrap().is_created());
        assert!(!resumed.account(&address).unwrap().is_destructed());
        assert_eq!(resumed.storage_slot_untracked(&address, &Word::ONE).unwrap(), Word::from(23));
        assert_eq!(
            resumed.storage_slot_untracked(&address, &Word::from(2)).unwrap(),
            Word::from(47)
        );

        source.set_pending_state(pending);
        source.storage(&address).unwrap().wipe();
        source.account(&address).unwrap().mark_destructed();
        let mut merged = resumed.take_pending_state();
        merged.merge_account(&source.take_pending_state(), &address);
        resumed.set_pending_state(merged);
        assert_eq!(resumed.storage_slot_untracked(&address, &Word::from(2)).unwrap(), Word::ZERO);
        resumed.finalize_transaction(Version::base(SpecId::PRAGUE)).unwrap();
        assert!(resumed.account_info_untracked(&address).unwrap().is_none());
    }
}
