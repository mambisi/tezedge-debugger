use std::{
    sync::{Arc, atomic::{Ordering, AtomicU64}},
    marker::PhantomData,
};
use rocksdb::{Cache, ColumnFamilyDescriptor, DB};
use storage::{
    Direction,
    IteratorMode,
    StorageError,
    persistent::{BincodeEncoded, KeyValueStoreWithSchema},
};
use super::{secondary_index::SecondaryIndices, remote::{KeyValueSchemaExt, ColumnFamilyDescriptorExt}};

pub trait MessageHasId {
    fn set_id(&mut self, id: u64);
}

pub trait StoreCollector {
    type Message: MessageHasId;

    fn store_message(&self, msg: Self::Message) -> Result<u64, StorageError>;

    /// Deletes the message and corresponding secondary indices.
    fn delete_message(&self, index: u64) -> Result<(), StorageError>;
}

/// generic message store
pub struct Store<KvStorage, Message, Schema, Indices>
where
    KvStorage: KeyValueStoreWithSchema<Schema> + AsRef<DB>,
    Message: BincodeEncoded + MessageHasId,
    Schema: KeyValueSchemaExt<Key = u64, Value = Message>,
    Indices: SecondaryIndices<PrimarySchema = Schema>,
{
    kv: Arc<KvStorage>,
    count: Arc<AtomicU64>,
    seq: Arc<AtomicU64>,
    limit: u64,
    indices: Indices,
    phantom_data: PhantomData<(Message, Schema)>,
}

impl<KvStorage, Message, Schema, Indices> Clone for Store<KvStorage, Message, Schema, Indices>
where
    KvStorage: KeyValueStoreWithSchema<Schema> + AsRef<DB>,
    Message: BincodeEncoded + MessageHasId,
    Schema: KeyValueSchemaExt<Key = u64, Value = Message>,
    Indices: SecondaryIndices<PrimarySchema = Schema> + Clone,
{
    fn clone(&self) -> Self {
        Store {
            kv: self.kv.clone(),
            count: self.count.clone(),
            seq: self.seq.clone(),
            limit: self.limit,
            indices: self.indices.clone(),
            phantom_data: PhantomData,
        }
    }
}

impl<KvStorage, Message, Schema, Indices> Store<KvStorage, Message, Schema, Indices>
where
    KvStorage: KeyValueStoreWithSchema<Schema> + AsRef<DB>,
    Message: BincodeEncoded + MessageHasId,
    Schema: KeyValueSchemaExt<Key = u64, Value = Message>,
    Indices: SecondaryIndices<PrimarySchema = Schema>,
{
    pub fn new(kv: &Arc<KvStorage>, indices: Indices, limit: u64) -> Self {
        Store {
            kv: kv.clone(),
            count: Arc::new(AtomicU64::new(0)),
            seq: Arc::new(AtomicU64::new(0)),
            limit,
            indices,
            phantom_data: PhantomData,
        }
    }

    pub fn schemas(cache: &Cache) -> impl Iterator<Item = ColumnFamilyDescriptor> {
        use std::iter;

        Indices::schemas(cache).into_iter().chain(iter::once(Schema::descriptor(cache)))
    }

    pub fn schemas_ext() -> impl Iterator<Item = ColumnFamilyDescriptorExt> {
        use std::iter;

        Indices::schemas_ext().into_iter().chain(iter::once(Schema::descriptor_ext()))
    }

    fn inner(&self) -> &impl KeyValueStoreWithSchema<Schema> {
        self.kv.as_ref()
    }

    // Increment count of messages in the store
    fn inc_count(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    // Reserve new index for later use.
    fn reserve_index(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Create iterator ending on given index. If no value is provided
    /// start at the end
    pub fn get_cursor(&self, cursor_index: Option<u64>, limit: usize, filter: &Indices::Filter) -> Result<Vec<Message>, StorageError> {
        let cursor_index = cursor_index.unwrap_or(u64::MAX);
        let ret = if let Some(keys) = self.indices.filter_iterator(&cursor_index, limit, &filter)? {
            keys.iter()
                .filter_map(move |index| {
                    match self.kv.get(&index) {
                        Ok(Some(value)) => {
                            Some(value)
                        },
                        Ok(None) => {
                            tracing::info!("No value at index: {}", index);
                            None
                        },
                        Err(err) => {
                            tracing::warn!("Failed to load value at index {}: {}", index, err);
                            None
                        },
                    }
                })
                .collect()
        } else {
            self.inner()
                .iterator(IteratorMode::From(&cursor_index, Direction::Reverse))?
                .filter_map(|(k, v)| {
                    match (k, v) {
                        (Ok(_), Ok(v)) => Some(v),
                        (Ok(index), Err(err)) => {
                            tracing::warn!("Failed to load value at index {}: {}", index, err);
                            None
                        },
                        (Err(err), _) => {
                            tracing::warn!("Failed to load index: {}", err);
                            None
                        },
                    }
                })
                .take(limit)
                .collect()
        };

        Ok(ret)
    }
}

impl<KvStorage, Message, Schema, Indices> StoreCollector for Store<KvStorage, Message, Schema, Indices>
where
    KvStorage: KeyValueStoreWithSchema<Schema> + AsRef<DB>,
    Message: BincodeEncoded + MessageHasId,
    Schema: KeyValueSchemaExt<Key = u64, Value = Message>,
    Indices: SecondaryIndices<PrimarySchema = Schema> + Clone,
{
    type Message = Message;

    fn store_message(&self, msg: Self::Message) -> Result<u64, StorageError> {
        let mut msg = msg;
        let index = self.reserve_index();
        if index >= self.limit {
            self.delete_message(index - self.limit)?;
        }
        msg.set_id(index);
        self.kv.put(&index, &msg)?;
        self.indices.store_indices(&index, &msg)?;
        self.inc_count();
        Ok(index)
    }

    fn delete_message(&self, index: u64) -> Result<(), StorageError> {
        if let Some(value) = self.kv.get(&index)? {
            self.indices.delete_indices(&index, &value)?;
            self.kv.delete(&index)?;
        }
        Ok(())
    }
}