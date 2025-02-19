#[cfg(test)]
use mock_instant::{
    Instant,
    SystemTime,
    UNIX_EPOCH,
};
#[cfg(not(test))]
use std::time::{
    Instant,
    SystemTime,
    UNIX_EPOCH,
};
use {
    self::{
        proof::wormhole_merkle::{
            construct_update_data,
            WormholeMerkleState,
        },
        storage::{
            MessageState,
            MessageStateFilter,
            Storage,
        },
        types::{
            AccumulatorMessages,
            PriceFeedUpdate,
            PriceFeedsWithUpdateData,
            RequestTime,
            Update,
        },
        wormhole::GuardianSet,
    },
    crate::store::{
        proof::wormhole_merkle::{
            construct_message_states_proofs,
            store_wormhole_merkle_verified_message,
        },
        types::{
            ProofSet,
            UnixTimestamp,
        },
        wormhole::verify_vaa,
    },
    anyhow::{
        anyhow,
        Result,
    },
    byteorder::BigEndian,
    pyth_sdk::PriceIdentifier,
    pythnet_sdk::{
        messages::{
            Message,
            MessageType,
        },
        wire::{
            from_slice,
            v1::{
                WormholeMessage,
                WormholePayload,
            },
        },
    },
    std::{
        collections::{
            BTreeMap,
            BTreeSet,
            HashSet,
        },
        sync::Arc,
        time::Duration,
    },
    tokio::sync::{
        mpsc::Sender,
        RwLock,
    },
    wormhole_sdk::{
        Address,
        Chain,
        Vaa,
    },
};

pub mod proof;
pub mod storage;
pub mod types;
pub mod wormhole;

const OBSERVED_CACHE_SIZE: usize = 1000;
const READINESS_STALENESS_THRESHOLD: Duration = Duration::from_secs(30);

pub struct Store {
    /// Storage is a short-lived cache of the state of all the updates
    /// that have been passed to the store.
    pub storage:                  Storage,
    /// Sequence numbers of lately observed Vaas. Store uses this set
    /// to ignore the previously observed Vaas as a performance boost.
    pub observed_vaa_seqs:        RwLock<BTreeSet<u64>>,
    /// Wormhole guardian sets. It is used to verify Vaas before using
    /// them.
    pub guardian_set:             RwLock<BTreeMap<u32, GuardianSet>>,
    /// The sender to the channel between Store and Api to notify
    /// completed updates.
    pub update_tx:                Sender<()>,
    /// Time of the last completed update. This is used for the health
    /// probes.
    pub last_completed_update_at: RwLock<Option<Instant>>,
}

impl Store {
    pub fn new(update_tx: Sender<()>, cache_size: u64) -> Arc<Self> {
        Arc::new(Self {
            storage: Storage::new(cache_size),
            observed_vaa_seqs: RwLock::new(Default::default()),
            guardian_set: RwLock::new(Default::default()),
            update_tx,
            last_completed_update_at: RwLock::new(None),
        })
    }

    /// Stores the update data in the store
    pub async fn store_update(&self, update: Update) -> Result<()> {
        // The slot that the update is originating from. It should be available
        // in all the updates.
        let slot = match update {
            Update::Vaa(vaa_bytes) => {
                // FIXME: Move to wormhole.rs
                let vaa =
                    serde_wormhole::from_slice::<Vaa<&serde_wormhole::RawMessage>>(&vaa_bytes)?;

                if vaa.emitter_chain != Chain::Pythnet
                    || vaa.emitter_address != Address(pythnet_sdk::ACCUMULATOR_EMITTER_ADDRESS)
                {
                    return Ok(()); // Ignore VAA from other emitters
                }

                if self.observed_vaa_seqs.read().await.contains(&vaa.sequence) {
                    return Ok(()); // Ignore VAA if we have already seen it
                }

                let vaa = verify_vaa(self, vaa).await;

                let vaa = match vaa {
                    Ok(vaa) => vaa,
                    Err(err) => {
                        log::info!("Ignoring invalid VAA: {:?}", err);
                        return Ok(());
                    }
                };

                {
                    let mut observed_vaa_seqs = self.observed_vaa_seqs.write().await;
                    observed_vaa_seqs.insert(vaa.sequence);
                    while observed_vaa_seqs.len() > OBSERVED_CACHE_SIZE {
                        observed_vaa_seqs.pop_first();
                    }
                }

                match WormholeMessage::try_from_bytes(vaa.payload)?.payload {
                    WormholePayload::Merkle(proof) => {
                        log::info!("Storing merkle proof for slot {:?}", proof.slot,);
                        store_wormhole_merkle_verified_message(self, proof.clone(), vaa_bytes)
                            .await?;
                        proof.slot
                    }
                }
            }
            Update::AccumulatorMessages(accumulator_messages) => {
                let slot = accumulator_messages.slot;
                log::info!("Storing accumulator messages for slot {:?}.", slot,);
                self.storage
                    .store_accumulator_messages(accumulator_messages)
                    .await?;
                slot
            }
        };

        let accumulator_messages = self.storage.fetch_accumulator_messages(slot).await?;
        let wormhole_merkle_state = self.storage.fetch_wormhole_merkle_state(slot).await?;

        let (accumulator_messages, wormhole_merkle_state) =
            match (accumulator_messages, wormhole_merkle_state) {
                (Some(accumulator_messages), Some(wormhole_merkle_state)) => {
                    (accumulator_messages, wormhole_merkle_state)
                }
                _ => return Ok(()),
            };

        // Once the accumulator reaches a complete state for a specific slot
        // we can build the message states
        self.build_message_states(accumulator_messages, wormhole_merkle_state)
            .await?;

        self.update_tx.send(()).await?;

        self.last_completed_update_at
            .write()
            .await
            .replace(Instant::now());

        Ok(())
    }

    async fn build_message_states(
        &self,
        accumulator_messages: AccumulatorMessages,
        wormhole_merkle_state: WormholeMerkleState,
    ) -> Result<()> {
        let wormhole_merkle_message_states_proofs =
            construct_message_states_proofs(&accumulator_messages, &wormhole_merkle_state)?;

        let current_time: UnixTimestamp =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as _;

        let message_states = accumulator_messages
            .raw_messages
            .into_iter()
            .enumerate()
            .map(|(idx, raw_message)| {
                Ok(MessageState::new(
                    from_slice::<BigEndian, _>(raw_message.as_ref())
                        .map_err(|e| anyhow!("Failed to deserialize message: {:?}", e))?,
                    raw_message,
                    ProofSet {
                        wormhole_merkle_proof: wormhole_merkle_message_states_proofs
                            .get(idx)
                            .ok_or(anyhow!("Missing proof for message"))?
                            .clone(),
                    },
                    accumulator_messages.slot,
                    current_time,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        log::info!("Message states len: {:?}", message_states.len());

        self.storage.store_message_states(message_states).await?;

        Ok(())
    }

    pub async fn update_guardian_set(&self, id: u32, guardian_set: GuardianSet) {
        let mut guardian_sets = self.guardian_set.write().await;
        guardian_sets.insert(id, guardian_set);
    }

    pub async fn get_price_feeds_with_update_data(
        &self,
        price_ids: Vec<PriceIdentifier>,
        request_time: RequestTime,
    ) -> Result<PriceFeedsWithUpdateData> {
        let messages = self
            .storage
            .fetch_message_states(
                price_ids
                    .iter()
                    .map(|price_id| price_id.to_bytes())
                    .collect(),
                request_time,
                MessageStateFilter::Only(MessageType::PriceFeedMessage),
            )
            .await?;

        let price_feeds = messages
            .iter()
            .map(|message_state| match message_state.message {
                Message::PriceFeedMessage(price_feed) => Ok(PriceFeedUpdate {
                    price_feed,
                    received_at: message_state.received_at,
                    slot: message_state.slot,
                    wormhole_merkle_update_data: construct_update_data(vec![message_state])?
                        .into_iter()
                        .next()
                        .ok_or(anyhow!("Missing update data for message"))?,
                }),
                _ => Err(anyhow!("Invalid message state type")),
            })
            .collect::<Result<Vec<_>>>()?;

        let update_data = construct_update_data(messages.iter().collect())?;

        Ok(PriceFeedsWithUpdateData {
            price_feeds,
            wormhole_merkle_update_data: update_data,
        })
    }

    pub async fn get_price_feed_ids(&self) -> HashSet<PriceIdentifier> {
        self.storage
            .message_state_keys()
            .await
            .iter()
            .map(|key| PriceIdentifier::new(key.feed_id))
            .collect()
    }

    pub async fn is_ready(&self) -> bool {
        let last_completed_update_at = self.last_completed_update_at.read().await;
        match last_completed_update_at.as_ref() {
            Some(last_completed_update_at) => {
                last_completed_update_at.elapsed() < READINESS_STALENESS_THRESHOLD
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::{
            types::Slot,
            *,
        },
        futures::future::join_all,
        mock_instant::MockClock,
        pythnet_sdk::{
            accumulators::{
                merkle::{
                    MerkleRoot,
                    MerkleTree,
                },
                Accumulator,
            },
            hashers::keccak256_160::Keccak160,
            messages::{
                Message,
                PriceFeedMessage,
            },
            wire::v1::{
                AccumulatorUpdateData,
                Proof,
                WormholeMerkleRoot,
            },
        },
        rand::seq::SliceRandom,
        serde_wormhole::RawMessage,
        tokio::sync::mpsc::Receiver,
    };

    /// Generate list of updates for the given list of messages at a given slot with given sequence
    ///
    /// Sequence in Vaas is used to filter duplicate messages (as by wormhole design there is only
    /// one message per sequence)
    pub fn generate_update(messages: Vec<Message>, slot: Slot, sequence: u64) -> Vec<Update> {
        let mut updates = Vec::new();

        // Accumulator messages
        let accumulator_messages = AccumulatorMessages {
            slot,
            raw_messages: messages
                .iter()
                .map(|message| pythnet_sdk::wire::to_vec::<_, byteorder::BE>(message).unwrap())
                .collect(),
            magic: [0; 4],
            ring_size: 100,
        };
        updates.push(Update::AccumulatorMessages(accumulator_messages.clone()));

        // Wormhole merkle update
        let merkle_tree = MerkleTree::<Keccak160>::from_set(
            accumulator_messages.raw_messages.iter().map(|m| m.as_ref()),
        )
        .unwrap();

        let wormhole_message = WormholeMessage::new(WormholePayload::Merkle(WormholeMerkleRoot {
            slot,
            ring_size: 100,
            root: merkle_tree.root.as_bytes().try_into().unwrap(),
        }));

        let wormhole_message =
            pythnet_sdk::wire::to_vec::<_, byteorder::BE>(&wormhole_message).unwrap();

        let vaa = Vaa {
            nonce: 0,
            version: 0,
            sequence,
            timestamp: 0,
            signatures: vec![],    // We are bypassing signature check now
            guardian_set_index: 0, // We are bypassing signature check now
            emitter_chain: Chain::Pythnet,
            emitter_address: Address(pythnet_sdk::ACCUMULATOR_EMITTER_ADDRESS),
            consistency_level: 0,
            payload: serde_wormhole::RawMessage::new(wormhole_message.as_ref()),
        };

        updates.push(Update::Vaa(serde_wormhole::to_vec(&vaa).unwrap()));

        updates
    }

    /// Create a dummy price feed base on the given seed for all the fields except
    /// `publish_time` and `prev_publish_time`. Those are set to the given value.
    pub fn create_dummy_price_feed_message(
        seed: u8,
        publish_time: i64,
        prev_publish_time: i64,
    ) -> PriceFeedMessage {
        PriceFeedMessage {
            feed_id: [seed; 32],
            price: seed as _,
            conf: seed as _,
            exponent: 0,
            ema_conf: seed as _,
            ema_price: seed as _,
            publish_time,
            prev_publish_time,
        }
    }

    pub async fn setup_store(cache_size: u64) -> (Arc<Store>, Receiver<()>) {
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(1000);
        let store = Store::new(update_tx, cache_size);

        // Add an initial guardian set with public key 0
        store
            .update_guardian_set(
                0,
                GuardianSet {
                    keys: vec![[0; 20]],
                },
            )
            .await;

        (store, update_rx)
    }

    pub async fn store_multiple_concurrent_valid_updates(store: Arc<Store>, updates: Vec<Update>) {
        let res = join_all(updates.into_iter().map(|u| store.store_update(u))).await;
        // Check that all store_update calls succeeded
        assert!(res.into_iter().all(|r| r.is_ok()));
    }

    #[tokio::test]
    pub async fn test_store_works() {
        let (store, mut update_rx) = setup_store(10).await;

        let price_feed_message = create_dummy_price_feed_message(100, 10, 9);

        // Populate the store
        store_multiple_concurrent_valid_updates(
            store.clone(),
            generate_update(vec![Message::PriceFeedMessage(price_feed_message)], 10, 20),
        )
        .await;

        // Check that the update_rx channel has received a message
        assert_eq!(update_rx.recv().await, Some(()));

        // Check the price ids are stored correctly
        assert_eq!(
            store.get_price_feed_ids().await,
            vec![PriceIdentifier::new([100; 32])].into_iter().collect()
        );

        // Check get_price_feeds_with_update_data retrieves the correct
        // price feed with correct update data.
        let price_feeds_with_update_data = store
            .get_price_feeds_with_update_data(
                vec![PriceIdentifier::new([100; 32])],
                RequestTime::Latest,
            )
            .await
            .unwrap();

        assert_eq!(
            price_feeds_with_update_data.price_feeds,
            vec![PriceFeedUpdate {
                price_feed:                  price_feed_message,
                slot:                        10,
                received_at:                 price_feeds_with_update_data.price_feeds[0]
                    .received_at, // Ignore checking this field.
                wormhole_merkle_update_data: price_feeds_with_update_data.price_feeds[0]
                    .wormhole_merkle_update_data
                    .clone(), // Ignore checking this field.
            }]
        );

        // Check the update data is correct.
        assert_eq!(
            price_feeds_with_update_data
                .wormhole_merkle_update_data
                .len(),
            1
        );
        let update_data = price_feeds_with_update_data
            .wormhole_merkle_update_data
            .get(0)
            .unwrap();
        let update_data = AccumulatorUpdateData::try_from_slice(update_data.as_ref()).unwrap();
        match update_data.proof {
            Proof::WormholeMerkle { vaa, updates } => {
                // Check the vaa and get the root
                let vaa: Vec<u8> = vaa.into();
                let vaa: Vaa<&RawMessage> = serde_wormhole::from_slice(vaa.as_ref()).unwrap();
                assert_eq!(
                    vaa,
                    Vaa {
                        nonce:              0,
                        version:            0,
                        sequence:           20,
                        timestamp:          0,
                        signatures:         vec![],
                        guardian_set_index: 0,
                        emitter_chain:      Chain::Pythnet,
                        emitter_address:    Address(pythnet_sdk::ACCUMULATOR_EMITTER_ADDRESS),
                        consistency_level:  0,
                        payload:            vaa.payload, // Ignore checking this field.
                    }
                );
                let merkle_root = WormholeMessage::try_from_bytes(vaa.payload.as_ref()).unwrap();
                let WormholePayload::Merkle(merkle_root) = merkle_root.payload;
                assert_eq!(
                    merkle_root,
                    WormholeMerkleRoot {
                        slot:      10,
                        ring_size: 100,
                        root:      merkle_root.root, // Ignore checking this field.
                    }
                );

                // Check the updates
                assert_eq!(updates.len(), 1);
                let update = updates.get(0).unwrap();
                let message: Vec<u8> = update.message.clone().into();
                // Check the serialized message is the price feed message generated above.
                assert_eq!(
                    pythnet_sdk::wire::from_slice::<byteorder::BE, Message>(message.as_ref())
                        .unwrap(),
                    Message::PriceFeedMessage(price_feed_message)
                );

                // Check the proof is correct with the Vaa root
                let merkle_root = MerkleRoot::<Keccak160>::new(merkle_root.root);
                assert!(merkle_root.check(update.proof.clone(), message.as_ref()));
            }
        }
    }

    #[tokio::test]
    pub async fn test_metadata_times_and_readiness_work() {
        // The receiver channel should stay open for the store to work
        // properly. That is why we don't use _ here as it drops the channel
        // immediately.
        let (store, _receiver_tx) = setup_store(10).await;

        let price_feed_message = create_dummy_price_feed_message(100, 10, 9);

        // Advance the clock
        MockClock::advance_system_time(Duration::from_secs(1));
        MockClock::advance(Duration::from_secs(1));

        // Get the current unix timestamp. It is mocked using
        // mock-instance module. So it should remain the same
        // on the next call.
        let unix_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Populate the store
        store_multiple_concurrent_valid_updates(
            store.clone(),
            generate_update(vec![Message::PriceFeedMessage(price_feed_message)], 10, 20),
        )
        .await;

        // Advance the clock again
        MockClock::advance_system_time(Duration::from_secs(1));
        MockClock::advance(Duration::from_secs(1));

        // Get the price feeds with update data
        let price_feeds_with_update_data = store
            .get_price_feeds_with_update_data(
                vec![PriceIdentifier::new([100; 32])],
                RequestTime::Latest,
            )
            .await
            .unwrap();

        // check received_at is correct
        assert_eq!(price_feeds_with_update_data.price_feeds.len(), 1);
        assert_eq!(
            price_feeds_with_update_data.price_feeds[0].received_at,
            unix_timestamp as i64
        );

        // Check the store is ready
        assert!(store.is_ready().await);

        // Advance the clock to make the prices stale
        MockClock::advance_system_time(READINESS_STALENESS_THRESHOLD);
        MockClock::advance(READINESS_STALENESS_THRESHOLD);
        // Check the store is not ready
        assert!(!store.is_ready().await);
    }

    /// Test that the store retains the latest slots upon cache eviction.
    ///
    /// Store is set up with cache size of 100 and 1000 slot updates will
    /// be stored all at the same time with random order.
    /// After the cache eviction, the store should retain the latest 100
    /// slots regardless of the order.
    #[tokio::test]
    pub async fn test_store_retains_latest_slots_upon_cache_eviction() {
        // The receiver channel should stay open for the store to work
        // properly. That is why we don't use _ here as it drops the channel
        // immediately.
        let (store, _receiver_tx) = setup_store(100).await;

        let mut updates: Vec<Update> = (0..1000)
            .flat_map(|slot| {
                let messages = vec![
                    Message::PriceFeedMessage(create_dummy_price_feed_message(
                        100,
                        slot as i64,
                        slot as i64,
                    )),
                    Message::PriceFeedMessage(create_dummy_price_feed_message(
                        200,
                        slot as i64,
                        slot as i64,
                    )),
                ];
                generate_update(messages, slot, slot)
            })
            .collect();

        // Shuffle the updates
        let mut rng = rand::thread_rng();
        updates.shuffle(&mut rng);

        // Store the updates
        store_multiple_concurrent_valid_updates(store.clone(), updates).await;

        // Check the last 100 slots are retained
        for slot in 900..1000 {
            let price_feeds_with_update_data = store
                .get_price_feeds_with_update_data(
                    vec![
                        PriceIdentifier::new([100; 32]),
                        PriceIdentifier::new([200; 32]),
                    ],
                    RequestTime::FirstAfter(slot as i64),
                )
                .await
                .unwrap();
            assert_eq!(price_feeds_with_update_data.price_feeds.len(), 2);
            assert_eq!(price_feeds_with_update_data.price_feeds[0].slot, slot);
            assert_eq!(price_feeds_with_update_data.price_feeds[1].slot, slot);
        }

        // Check nothing else is retained
        for slot in 0..900 {
            assert!(store
                .get_price_feeds_with_update_data(
                    vec![
                        PriceIdentifier::new([100; 32]),
                        PriceIdentifier::new([200; 32]),
                    ],
                    RequestTime::FirstAfter(slot as i64),
                )
                .await
                .is_err());
        }
    }
}
