//! In memory network simulator
//!
//! This module provides an in-memory only simulation of an actual network, useful for unit and
//! integration tests.

use super::{FailedToSerializeSnafu, NetworkError, NetworkReliability, NetworkingImplementation};
use crate::PubKey;
use async_std::sync::RwLock;
use async_std::task::spawn;
use async_trait::async_trait;
use bincode::Options;
use dashmap::DashMap;
use futures::StreamExt;
use phaselock_types::traits::network::NetworkChange;
use rand::Rng;
use serde::Deserialize;
use serde::{de::DeserializeOwned, Serialize};
use snafu::ResultExt;
use std::fmt::Debug;
use std::sync::Arc;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};

#[derive(Debug, Clone, Copy)]
/// dummy implementation of network reliability
pub struct DummyReliability {}
impl NetworkReliability for DummyReliability {
    fn sample_keep(&self) -> bool {
        true
    }
    fn sample_delay(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

/// Shared state for in-memory mock networking.
///
/// This type is responsible for keeping track of the channels to each [`MemoryNetwork`], and is
/// used to group the [`MemoryNetwork`] instances.
#[derive(custom_debug::Debug)]
pub struct MasterMap<T> {
    /// The list of `MemoryNetwork`s
    #[debug(skip)]
    map: DashMap<PubKey, MemoryNetwork<T>>,
    /// The id of this `MemoryNetwork` cluster
    id: u64,
}

impl<T> MasterMap<T> {
    /// Create a new, empty, `MasterMap`
    pub fn new() -> Arc<MasterMap<T>> {
        Arc::new(MasterMap {
            map: DashMap::new(),
            id: rand::thread_rng().gen(),
        })
    }
}

/// Internal enum for combining streams
enum Combo<T> {
    /// Direct message
    Direct(T),
    /// Broadcast message
    Broadcast(T),
}

/// Internal state for a `MemoryNetwork` instance
struct MemoryNetworkInner<T> {
    /// The public key of this node
    pub_key: PubKey,
    /// Input for broadcast messages
    broadcast_input: RwLock<Option<flume::Sender<Vec<u8>>>>,
    /// Input for direct messages
    direct_input: RwLock<Option<flume::Sender<Vec<u8>>>>,
    /// Output for broadcast messages
    broadcast_output: flume::Receiver<T>,
    /// Output for direct messages
    direct_output: flume::Receiver<T>,
    /// The master map
    master_map: Arc<MasterMap<T>>,

    /// Input for network change messages
    network_changes_input: RwLock<Option<flume::Sender<NetworkChange>>>,
    /// Output for network change messages
    network_changes_output: flume::Receiver<NetworkChange>,
}

/// In memory only network simulator.
///
/// This provides an in memory simulation of a networking implementation, allowing nodes running on
/// the same machine to mock networking while testing other functionality.
///
/// Under the hood, this simply maintains mpmc channels to every other `MemoryNetwork` insane of the
/// same group.
#[derive(Clone)]
pub struct MemoryNetwork<T> {
    /// The actual internal state
    inner: Arc<MemoryNetworkInner<T>>,
}

impl<T> Debug for MemoryNetwork<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryNetwork")
            .field("inner", &"inner")
            .finish()
    }
}

impl<T> MemoryNetwork<T>
where
    T: Clone + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug + 'static,
{
    /// Creates a new `MemoryNetwork` and hooks it up to the group through the provided `MasterMap`
    #[instrument]
    pub fn new(
        pub_key: PubKey,
        master_map: Arc<MasterMap<T>>,
        reliability_config: Option<Arc<dyn 'static + NetworkReliability>>,
    ) -> MemoryNetwork<T> {
        info!("Attaching new MemoryNetwork");
        let (broadcast_input, broadcast_task_recv) = flume::bounded(128);
        let (direct_input, direct_task_recv) = flume::bounded(128);
        let (broadcast_task_send, broadcast_output) = flume::bounded(128);
        let (direct_task_send, direct_output) = flume::bounded(128);
        let (network_changes_input, network_changes_output) = flume::bounded(128);
        trace!("Channels open, spawning background task");

        spawn(
            async move {
                debug!("Starting background task");
                // Use the same wire format as WNetwork to make sure round tripping is simulated
                let bincode_options = bincode::DefaultOptions::new().with_limit(16_384);
                // direct input is right stream
                let direct = direct_task_recv.into_stream().map(Combo::<Vec<u8>>::Direct);
                // broadcast input is left stream
                let broadcast = broadcast_task_recv
                    .into_stream()
                    .map(Combo::<Vec<u8>>::Broadcast);
                // Combine the streams
                let mut combined = futures::stream::select(direct, broadcast);
                trace!("Entering processing loop");
                while let Some(message) = combined.next().await {
                    match message {
                        Combo::Direct(vec) => {
                            trace!(?vec, "Incoming direct message");
                            // Attempt to decode message
                            let x = bincode_options.deserialize(&vec);
                            match x {
                                Ok(x) => {
                                    let dts = direct_task_send.clone();
                                    if let Some(r) = reliability_config.clone() {
                                        spawn(async move {
                                            if r.sample_keep() {
                                                let delay = r.sample_delay();
                                                if delay > std::time::Duration::ZERO {
                                                    async_std::task::sleep(delay).await;
                                                }
                                                let res = dts.send_async(x).await;
                                                if res.is_ok() {
                                                    trace!("Passed message to output queue");
                                                } else {
                                                    error!("Output queue receivers are shutdown");
                                                }
                                            } else {
                                                warn!("dropping packet!");
                                            }
                                        });
                                    } else {
                                        let res = dts.send_async(x).await;
                                        if res.is_ok() {
                                            trace!("Passed message to output queue");
                                        } else {
                                            error!("Output queue receivers are shutdown");
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(?e, "Failed to decode incoming message, skipping");
                                }
                            }
                        }
                        Combo::Broadcast(vec) => {
                            trace!(?vec, "Incoming broadcast message");
                            // Attempt to decode message
                            let x = bincode_options.deserialize(&vec);
                            match x {
                                Ok(x) => {
                                    let bts = broadcast_task_send.clone();
                                    if let Some(r) = reliability_config.clone() {
                                        spawn(async move {
                                            if r.sample_keep() {
                                                let delay = r.sample_delay();
                                                if delay > std::time::Duration::ZERO {
                                                    async_std::task::sleep(delay).await;
                                                }
                                                let res = bts.send_async(x).await;
                                                if res.is_ok() {
                                                    trace!("Passed message to output queue");
                                                } else {
                                                    warn!("dropping packet!");
                                                }
                                            }
                                        });
                                    } else {
                                        let res = bts.send_async(x).await;
                                        if res.is_ok() {
                                            trace!("Passed message to output queue");
                                        } else {
                                            warn!("dropping packet!");
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(?e, "Failed to decode incoming message, skipping");
                                }
                            }
                        }
                    }
                }
                error!("Stream shutdown");
            }
            .instrument(
                info_span!("MemoryNetwork Background task", id = ?pub_key.nonce, map = ?master_map),
            ),
        );
        trace!("Notifying other networks of the new connected peer");
        for other in master_map.map.iter() {
            async_std::task::block_on(
                other
                    .value()
                    .network_changes_input(NetworkChange::NodeConnected(pub_key.clone())),
            )
            .expect("Could not deliver message");
        }
        trace!("Task spawned, creating MemoryNetwork");
        let mn = MemoryNetwork {
            inner: Arc::new(MemoryNetworkInner {
                pub_key: pub_key.clone(),
                broadcast_input: RwLock::new(Some(broadcast_input)),
                direct_input: RwLock::new(Some(direct_input)),
                broadcast_output,
                direct_output,
                master_map: master_map.clone(),
                network_changes_input: RwLock::new(Some(network_changes_input)),
                network_changes_output,
            }),
        };
        master_map.map.insert(pub_key, mn.clone());
        trace!("Master map updated");

        mn
    }

    /// Send a [`Vec<u8>`] message to the inner `broadcast_input`
    async fn broadcast_input(&self, message: Vec<u8>) -> Result<(), flume::SendError<Vec<u8>>> {
        let input = self.inner.broadcast_input.read().await;
        if let Some(input) = &*input {
            input.send_async(message).await
        } else {
            Err(flume::SendError(message))
        }
    }

    /// Send a [`Vec<u8>`] message to the inner `direct_input`
    async fn direct_input(&self, message: Vec<u8>) -> Result<(), flume::SendError<Vec<u8>>> {
        let input = self.inner.direct_input.read().await;
        if let Some(input) = &*input {
            input.send_async(message).await
        } else {
            Err(flume::SendError(message))
        }
    }

    /// Send a [`NetworkChange`] message to the inner `network_changes_input`
    async fn network_changes_input(
        &self,
        message: NetworkChange,
    ) -> Result<(), flume::SendError<NetworkChange>> {
        let input = self.inner.network_changes_input.read().await;
        if let Some(input) = &*input {
            input.send_async(message).await
        } else {
            Err(flume::SendError(message))
        }
    }
}

#[async_trait]
impl<T: Clone + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug + 'static>
    NetworkingImplementation<T> for MemoryNetwork<T>
{
    #[instrument(
        name="MemoryNetwork::broadcast_message",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn broadcast_message(&self, message: T) -> Result<(), NetworkError> {
        debug!(?message, "Broadcasting message");
        // Bincode the message
        let bincode_options = bincode::DefaultOptions::new().with_limit(16_384);
        let vec = bincode_options
            .serialize(&message)
            .context(FailedToSerializeSnafu)?;
        trace!("Message bincoded, sending");
        for node in self.inner.master_map.map.iter() {
            let (key, node) = node.pair();
            trace!(?key, "Sending message to node");
            let res = node.broadcast_input(vec.clone()).await;
            match res {
                Ok(_) => trace!(?key, "Delivered message to remote"),
                Err(e) => {
                    error!(?e, ?key, "Error sending broadcast message to node");
                    return Err(NetworkError::CouldNotDeliver);
                }
            }
        }
        Ok(())
    }

    #[instrument(
        name="MemoryNetwork::ready",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn ready(&self) -> bool {
        true
    }

    #[instrument(
        name="MemoryNetwork::message_node",
        fields(node_id = ?self.inner.pub_key.nonce, other = recipient.nonce)
    )]
    async fn message_node(&self, message: T, recipient: PubKey) -> Result<(), NetworkError> {
        debug!(?message, ?recipient, "Sending direct message");
        // Bincode the message
        let bincode_options = bincode::DefaultOptions::new().with_limit(16_384);
        let vec = bincode_options
            .serialize(&message)
            .context(FailedToSerializeSnafu)?;
        trace!("Message bincoded, finding recipient");
        if let Some(node) = self.inner.master_map.map.get(&recipient) {
            let node = node.value();
            let res = node.direct_input(vec).await;
            match res {
                Ok(_) => {
                    trace!(?recipient, "Delivered message to remote");
                    Ok(())
                }
                Err(e) => {
                    error!(?e, ?recipient, "Error delivering direct message");
                    Err(NetworkError::CouldNotDeliver)
                }
            }
        } else {
            error!(?recipient, ?self.inner.master_map.map, "Node does not exist in map");
            Err(NetworkError::NoSuchNode)
        }
    }

    #[instrument(
        name="MemoryNetwork::broadcast_queue",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn broadcast_queue(&self) -> Result<Vec<T>, NetworkError> {
        debug!("Waiting for messages to show up");
        let mut ret = Vec::new();
        // Wait for the first message to come up
        let first = self.inner.broadcast_output.recv_async().await;
        if let Ok(first) = first {
            trace!(?first, "First message in broadcast queue found");
            ret.push(first);
            while let Ok(x) = self.inner.broadcast_output.try_recv() {
                ret.push(x);
            }
            Ok(ret)
        } else {
            error!("The underlying MemoryNetwork has shut down");
            Err(NetworkError::ShutDown)
        }
    }

    #[instrument(
        name="MemoryNetwork::next_broadcast",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn next_broadcast(&self) -> Result<T, NetworkError> {
        debug!("Awaiting next broadcast");
        let x = self.inner.broadcast_output.recv_async().await;
        if let Ok(x) = x {
            trace!(?x, "Found broadcast");
            Ok(x)
        } else {
            error!("The underlying MemoryNetwork has shutdown");
            Err(NetworkError::ShutDown)
        }
    }

    #[instrument(
        name="MemoryNetwork::direct_queue",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn direct_queue(&self) -> Result<Vec<T>, NetworkError> {
        debug!("Waiting for messages to show up");
        let mut ret = Vec::new();
        // Wait for the first message to come up
        let first = self.inner.direct_output.recv_async().await;
        if let Ok(first) = first {
            trace!(?first, "First message in direct queue found");
            ret.push(first);
            while let Ok(x) = self.inner.direct_output.try_recv() {
                ret.push(x);
            }
            Ok(ret)
        } else {
            error!("The underlying MemoryNetwork has shut down");
            Err(NetworkError::ShutDown)
        }
    }

    #[instrument(
        name="MemoryNetwork::next_direct",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn next_direct(&self) -> Result<T, NetworkError> {
        debug!("Awaiting next direct");
        let x = self.inner.direct_output.recv_async().await;
        if let Ok(x) = x {
            trace!(?x, "Found direct");
            Ok(x)
        } else {
            error!("The underlying MemoryNetwork has shutdown");
            Err(NetworkError::ShutDown)
        }
    }

    async fn known_nodes(&self) -> Vec<PubKey> {
        self.inner
            .master_map
            .map
            .iter()
            .map(|x| x.key().clone())
            .collect()
    }

    #[instrument(
        name="MemoryNetwork::network_changes",
        fields(node_id = ?self.inner.pub_key.nonce)
    )]
    async fn network_changes(&self) -> Result<Vec<NetworkChange>, NetworkError> {
        debug!("Waiting for network changes to show up");
        let mut ret = Vec::new();
        // Wait for the first message to come up
        let first = self.inner.network_changes_output.recv_async().await;
        if let Ok(first) = first {
            trace!(?first, "First message in network changes queue found");
            ret.push(first);
            while let Ok(x) = self.inner.network_changes_output.try_recv() {
                ret.push(x);
            }
            Ok(ret)
        } else {
            error!("The underlying MemoryNetwork has shut down");
            Err(NetworkError::ShutDown)
        }
    }

    async fn shut_down(&self) {
        *self.inner.broadcast_input.write().await = None;
        *self.inner.direct_input.write().await = None;
        *self.inner.network_changes_input.write().await = None;
    }

    async fn put_record(
        &self,
        _key: impl Serialize + Send + Sync + 'static,
        _value: impl Serialize + Send + Sync + 'static,
    ) -> Result<(), NetworkError> {
        unimplemented!()
    }

    async fn get_record<V: for<'a> Deserialize<'a>>(
        &self,
        _key: impl Serialize + Send + Sync + 'static,
    ) -> Result<V, NetworkError> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phaselock_utils::test_util::setup_logging;
    use serde::Deserialize;

    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Test {
        message: u64,
    }

    fn get_pubkey() -> PubKey {
        PubKey::random(rand::thread_rng().gen())
    }

    // Spawning a single MemoryNetwork should produce no errors
    #[test]
    #[instrument]
    fn spawn_single() {
        setup_logging();
        let group: Arc<MasterMap<Test>> = MasterMap::new();
        trace!(?group);
        let pub_key = get_pubkey();
        let _network = MemoryNetwork::new(pub_key, group, Option::None);
    }

    // Spawning a two MemoryNetworks and connecting them should produce no errors
    #[test]
    #[instrument]
    fn spawn_double() {
        setup_logging();
        let group: Arc<MasterMap<Test>> = MasterMap::new();
        trace!(?group);
        let pub_key_1 = get_pubkey();
        let _network_1 = MemoryNetwork::new(pub_key_1, group.clone(), Option::None);
        let pub_key_2 = get_pubkey();
        let _network_2 = MemoryNetwork::new(pub_key_2, group, Option::None);
    }

    // Check to make sure direct queue works
    #[async_std::test]
    #[instrument]
    async fn direct_queue() {
        setup_logging();
        // Create some dummy messages
        let messages: Vec<Test> = (0..5).map(|x| Test { message: x }).collect();
        // Make and connect the networking instances
        let group: Arc<MasterMap<Test>> = MasterMap::new();
        trace!(?group);
        let pub_key_1 = get_pubkey();
        let network1 = MemoryNetwork::new(pub_key_1.clone(), group.clone(), Option::None);
        let pub_key_2 = get_pubkey();
        let network2 = MemoryNetwork::new(pub_key_2.clone(), group, Option::None);

        // Test 1 -> 2
        // Send messages
        for message in &messages {
            network1
                .message_node(message.clone(), pub_key_2.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network2
                .next_direct()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);

        // Test 2 -> 1
        // Send messages
        for message in &messages {
            network2
                .message_node(message.clone(), pub_key_1.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network1
                .next_direct()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);
    }

    // Check to make sure direct queue works
    #[async_std::test]
    #[instrument]
    async fn broadcast_queue() {
        setup_logging();
        // Create some dummy messages
        let messages: Vec<Test> = (0..5).map(|x| Test { message: x }).collect();
        // Make and connect the networking instances
        let group: Arc<MasterMap<Test>> = MasterMap::new();
        trace!(?group);
        let pub_key_1 = get_pubkey();
        let network1 = MemoryNetwork::new(pub_key_1.clone(), group.clone(), Option::None);
        let pub_key_2 = get_pubkey();
        let network2 = MemoryNetwork::new(pub_key_2.clone(), group, Option::None);

        // Test 1 -> 2
        // Send messages
        for message in &messages {
            network1
                .broadcast_message(message.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network2
                .next_broadcast()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);

        // Test 2 -> 1
        // Send messages
        for message in &messages {
            network2
                .broadcast_message(message.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network1
                .next_broadcast()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);
    }
}
