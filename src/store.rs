use crate::ttl::{ExpirationMap, Time};
use parking_lot::{RwLock};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::mem;
use std::convert::TryInto;
use std::fmt::{Debug, Formatter};
use crate::utils::{change_lifetime_const, SharedValue, ValueRef, ValueRefMut};

const NUM_OF_SHARDS: usize = 256;

pub(crate) struct StoreItem<V> {
    key: u64,
    conflict: u64,
    value: SharedValue<V>,
    expiration: Time,
}

impl<V> Debug for StoreItem<V> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreItem")
            .field("key", &self.key)
            .field("conflict", &self.conflict)
            .field("expiration", &self.expiration)
            .finish()
    }
}

pub(crate) struct ShardedMap<V, DS = RandomState, S = RandomState> {
    shards: Box<[RwLock<HashMap<u64, StoreItem<V>, DS>>; NUM_OF_SHARDS]>,
    em: ExpirationMap<S>,
}

impl<V> ShardedMap<V> {
    pub fn new() -> Self {
        let em = ExpirationMap::new();

        let shards = Box::new((0..NUM_OF_SHARDS).map(|_| RwLock::new(HashMap::new())).collect::<Vec<_>>().try_into().unwrap());


        Self {
            shards,
            em,
        }
    }

    pub fn get(&self, key: u64, conflict: u64) -> Option<ValueRef<'_, V>> {
        let data = self.shards[(key as usize) % NUM_OF_SHARDS].read();

        if let Some(item) =  data.get(&key) {
            if conflict != 0 && (conflict != item.conflict) {
                return None;
            }

            // Handle expired items
            if !item.expiration.is_zero() && item.expiration.is_expired() {
                return None;
            }

            unsafe {
                let vptr = change_lifetime_const(item.value.get());
                Some(ValueRef::new(data, vptr))
            }
        } else {
            None
        }
    }

    pub fn get_mut(&self, key: u64, conflict: u64) -> Option<ValueRefMut<'_, V>> {
        let data = self.shards[(key as usize) % NUM_OF_SHARDS].write();

        if let Some(item) =  data.get(&key) {
            if conflict != 0 && (conflict != item.conflict) {
                return None;
            }

            // Handle expired items
            if !item.expiration.is_zero() && item.expiration.is_expired() {
                return None;
            }

            unsafe {
                let vptr = &mut *item.value.as_ptr();
                Some(ValueRefMut::new(data, vptr))
            }
        } else {
            None
        }
    }

    pub fn insert(&self, key: u64, val: V, conflict: u64, expiration: Time) {
        let mut data = self.shards[(key as usize) % NUM_OF_SHARDS].write();

        match data.get(&key) {
            None => {
                // The value is not in the map already. There's no need to return anything.
                // Simply add the expiration map.
                self.em.insert(key, conflict, expiration);
            }
            Some(sitem) => {
                // The item existed already. We need to check the conflict key and reject the
                // update if they do not match. Only after that the expiration map is updated.
                if conflict != 0 && (conflict != sitem.conflict) {
                    return;
                }

                //TODO: should update check
                self.em.update(key, conflict, sitem.expiration, expiration);
            }
        }

        data.insert(
            key,
            StoreItem {
                key,
                conflict,
                value: SharedValue::new(val),
                expiration,
            },
        );
    }

    pub fn update(&self, key: u64, mut val: V, conflict: u64, expiration: Time) -> Option<V> {

        let mut data = self.shards[(key as usize) % NUM_OF_SHARDS].write();
        match data.get_mut(&key) {
            None => None,
            Some(item) => {
                if conflict != 0 && (conflict != item.conflict) {
                    return None;
                }

                // TODO: check update
                mem::swap(&mut val, &mut item.value.get_mut());

                //TODO: should update check
                self.em.update(key, conflict, item.expiration, expiration);

                Some(val)
            }
        }
    }

    pub fn remove(&self, key: u64, conflict: u64) -> (u64, Option<V>) {
        let mut data = self.shards[(key as usize) % NUM_OF_SHARDS].write();

        match data.get(&key) {
            None => (0, None),
            Some(item) => {
                if conflict != 0 && (conflict != item.conflict) {
                    return (0, None);
                }

                if !item.expiration.is_zero() {
                    self.em.remove(key, item.expiration);
                }

                data
                    .remove(&key)
                    .map_or((0, None), |item| (item.conflict, Some(item.value.into_inner())))
            }
        }
    }

    pub fn expiration(&self, key: u64) -> Option<Time> {
        self.shards[(key as usize) % NUM_OF_SHARDS].read().get(&key).map(|val| val.expiration)
    }

    pub fn clear(&self) {
        // TODO: item call back
        self.shards.iter().for_each(|shard| {
            shard.write().clear()
        });
    }


}

#[cfg(test)]
mod test {
    use crate::store::{ShardedMap};
    use std::sync::Arc;
    use crate::ttl::Time;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_store() {
        let _s: ShardedMap<u64> = ShardedMap::new();
    }

    #[test]
    fn test_store_set_get() {
        let s: ShardedMap<u64> = ShardedMap::new();

        s.insert(1, 2, 0, Time::now());
        let val = s.get(1, 0).unwrap();
        assert_eq!(&2, val.value());
        val.release();

        let mut val = s.get_mut(1, 0).unwrap();
        *val.value_mut() = 3;
        val.release();

        let v = s.get(1, 0).unwrap();
        assert_eq!(&3, v.value());
    }


    #[test]
    fn test_concurrent_get_insert() {
        let s = Arc::new(ShardedMap::new());
        let s1 = s.clone();

        std::thread::spawn(move || {
            s.insert(1, 2, 0, Time::now());
        });
        sleep(Duration::from_millis(10));
        assert_eq!(s1.get(1, 0).unwrap().read(), 2);
    }


    #[test]
    fn test_concurrent_get_mut_insert() {
        let s = Arc::new(ShardedMap::new());
        let s1 = s.clone();

        std::thread::spawn(move || {
            s.insert(1, 2, 0, Time::now());
            sleep(Duration::from_millis(20));
            assert_eq!(s.get(1, 0).unwrap().read(), 7);
        });
        sleep(Duration::from_millis(10));
        assert_eq!(s1.get(1, 0).unwrap().read(), 2);
        s1.get_mut(1, 0).unwrap().write(7);
    }

    #[test]
    fn test_store_remove() {
        let s: ShardedMap<u64> = ShardedMap::new();

        s.insert(1, 2, 0, Time::now());
        assert_eq!(s.remove(1, 0), (0, Some(2)));
        let v = s.get(1, 0);
        assert!(v.is_none());
        assert_eq!(s.remove(2, 0), (0, None));
    }
}