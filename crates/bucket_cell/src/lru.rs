// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a bounded-clock LRU map backed by `BTreeMap` so lookup, eviction, prefix/range removal,
//! and LRU-order iteration all share one deterministic implementation (board 6026).

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::ops::RangeBounds;

/// A map tracking each entry's last-use clock for O(log n) LRU eviction and deterministic ordering.
pub struct LruMap<K, V> {
    entries: BTreeMap<K, (V, u64)>,
    by_use: BTreeMap<u64, K>,
    clock: u64,
}

impl<K, V> Default for LruMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> LruMap<K, V> {
    pub const fn new() -> Self {
        Self { entries: BTreeMap::new(), by_use: BTreeMap::new(), clock: 0 }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Advances and returns the internal monotonic clock.
    pub fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Iterates entries from least recently used to most recently used.
    pub fn iter_lru(&self) -> impl Iterator<Item = (&K, &V)>
    where
        K: Ord,
    {
        self.by_use.values().filter_map(|key| self.entries.get(key).map(|(value, _)| (key, value)))
    }

    /// Iterates keys in key order.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    /// Consumes the map and yields values from least recently used to most recently used.
    pub fn into_lru_values(mut self) -> Vec<V>
    where
        K: Ord,
    {
        let mut values = Vec::with_capacity(self.entries.len());
        for (_, key) in std::mem::take(&mut self.by_use) {
            if let Some((value, _)) = self.entries.remove(&key) {
                values.push(value);
            }
        }
        values
    }
}

impl<K: Ord + Clone, V> LruMap<K, V> {
    /// Looks up `key`, marking it most recently used.
    pub fn get<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.get_mut(key).map(|value| &*value)
    }

    /// Looks up `key` mutably, marking it most recently used.
    pub fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let stored_key = self.entries.get_key_value(key)?.0.clone();
        let (value, used) = self.entries.get_mut(key)?;
        self.clock += 1;
        self.by_use.remove(used);
        *used = self.clock;
        self.by_use.insert(self.clock, stored_key);
        Some(value)
    }

    /// Looks up `key` without changing its recency.
    pub fn peek<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.entries.get(key).map(|(value, _)| value)
    }

    /// Looks up `key` mutably without changing its recency.
    pub fn peek_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        self.entries.get_mut(key).map(|(value, _)| value)
    }

    /// Inserts `key` -> `value` as most recently used, returning any displaced value.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.clock += 1;
        let replaced = self.entries.insert(key.clone(), (value, self.clock)).map(|(old, used)| {
            self.by_use.remove(&used);
            old
        });
        self.by_use.insert(self.clock, key);
        replaced
    }

    /// Removes `key`, returning its value if present.
    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let (value, used) = self.entries.remove(key)?;
        self.by_use.remove(&used);
        Some(value)
    }

    /// Peeks at the least recently used entry without removing it.
    pub fn peek_oldest(&self) -> Option<(&K, &V)> {
        let key = self.by_use.values().next()?;
        let (value, _) = self.entries.get(key)?;
        Some((key, value))
    }

    /// Removes and returns the least recently used entry.
    pub fn pop_oldest(&mut self) -> Option<(K, V)> {
        let (_, key) = self.by_use.pop_first()?;
        let (value, _) = self.entries.remove(&key)?;
        Some((key, value))
    }

    /// Removes all entries whose keys fall within `range`, returning their values.
    pub fn remove_range<R: RangeBounds<K>>(&mut self, range: R) -> Vec<V> {
        let keys: Vec<K> = self.entries.range(range).map(|(k, _)| k.clone()).collect();
        keys.into_iter().filter_map(|key| self.remove(&key)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::LruMap;

    #[test]
    fn lru_orders_evicts_and_removes_ranges() {
        let mut map = LruMap::new();
        map.insert((1u64, 0u64), "a");
        map.insert((1, 1), "b");
        map.insert((2, 0), "c");
        assert_eq!(map.get(&(1, 0)), Some(&"a"));
        assert_eq!(map.pop_oldest(), Some(((1, 1), "b")));
        let removed = map.remove_range((1, 0)..=(1, u64::MAX));
        assert_eq!(removed, vec!["a"]);
        assert_eq!(map.len(), 1);
        assert_eq!(map.into_lru_values(), vec!["c"]);
    }
}
