use std::{collections::hash_map::Entry, sync::Arc};

use alloy::primitives::{map::HashMap, U256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Storage {
    Root(HashMap<U256, U256>),
    // Provider(DynProvider, Address),
    Overlay(Overlay),
}

impl Default for Storage {
    fn default() -> Self {
        Storage::Root(Default::default())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Overlay {
    pub(crate) overlay: HashMap<U256, U256>,
    pub(crate) underlying: Arc<Storage>,
}

impl Storage {
    pub fn get(&self, slot: U256) -> U256 {
        match self {
            Storage::Root(map) => map.get(&slot).cloned().unwrap_or_default(),
            // Storage::Provider(dyn_provider, address) => dyn_provider.get_storage_at(*address, slot),
            Storage::Overlay(overlay) => overlay.overlay.get(&slot).cloned().unwrap_or_default(),
        }
    }

    pub fn insert(&mut self, slot: U256, data: U256) {
        match self {
            Storage::Root(map) => map.insert(slot, data),
            // Storage::Provider(dyn_provider, address) => dyn_provider.get_storage_at(*address, slot),
            Storage::Overlay(overlay) => overlay.overlay.insert(slot, data),
        };
    }

    pub fn remove(&mut self, slot: U256) {
        match self {
            Storage::Root(map) => map.remove(&slot),
            // Storage::Provider(dyn_provider, address) => dyn_provider.get_storage_at(*address, slot),
            Storage::Overlay(overlay) => overlay.overlay.remove(&slot),
        };
    }

    pub fn get_multiple(&self, slot: U256, length: usize) -> Vec<U256> {
        let mut slots = vec![];
        for i in 0..length {
            slots.push(self.get(slot + U256::from(i)));
        }
        slots
    }

    pub fn insert_multiple(&mut self, slot: U256, data: &[U256]) {
        for (i, value) in data.into_iter().enumerate() {
            self.insert(slot + U256::from(i), *value);
        }
    }

    pub fn entry(&mut self, slot: U256) -> Entry<'_, U256, U256> {
        match self {
            Storage::Root(map) => map.entry(slot),
            Storage::Overlay(overlay) => match overlay.overlay.entry(slot) {
                Entry::Vacant(vacant_entry) => match overlay.underlying.get(slot) {
                    U256::ZERO => Entry::Vacant(vacant_entry),
                    value => Entry::Occupied(vacant_entry.insert_entry(value)),
                },
                entry => entry,
            },
        }
    }
}
