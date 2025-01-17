// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Crate used for testing with Cumulus.

#![warn(missing_docs)]

mod chain_spec;
mod genesis;

use core::future::Future;
use cumulus_client_network::BlockAnnounceValidator;
use cumulus_client_service::{
	prepare_node_config, start_collator, start_full_node, StartCollatorParams, StartFullNodeParams,
};
use cumulus_primitives_core::ParaId;
use cumulus_test_runtime::{NodeBlock as Block, RuntimeApi};
use polkadot_primitives::v1::CollatorPair;
use sc_client_api::execution_extensions::ExecutionStrategies;
use sc_executor::native_executor_instance;
pub use sc_executor::NativeExecutor;
use sc_network::{config::TransportConfig, multiaddr, NetworkService};
use sc_service::{
	config::{
		DatabaseConfig, KeepBlocks, KeystoreConfig, MultiaddrWithPeerId, NetworkConfiguration,
		OffchainWorkerConfig, PruningMode, TransactionStorageMode, WasmExecutionMethod,
	},
	BasePath, ChainSpec, Configuration, Error as ServiceError, PartialComponents, Role,
	RpcHandlers, TFullBackend, TFullClient, TaskExecutor, TaskManager,
};
use sp_core::{Pair, H256};
use sp_keyring::Sr25519Keyring;
use sp_runtime::traits::BlakeTwo256;
use sp_state_machine::BasicExternalities;
use sp_trie::PrefixedMemoryDB;
use std::sync::Arc;
use substrate_test_client::BlockchainEventsExt;

pub use chain_spec::*;
pub use cumulus_test_runtime as runtime;
pub use genesis::*;
pub use sp_keyring::Sr25519Keyring as Keyring;

// Native executor instance.
native_executor_instance!(
	pub RuntimeExecutor,
	cumulus_test_runtime::api::dispatch,
	cumulus_test_runtime::native_version,
);

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
pub fn new_partial(
	config: &mut Configuration,
) -> Result<
	PartialComponents<
		TFullClient<Block, RuntimeApi, RuntimeExecutor>,
		TFullBackend<Block>,
		(),
		sp_consensus::import_queue::BasicQueue<Block, PrefixedMemoryDB<BlakeTwo256>>,
		sc_transaction_pool::FullPool<Block, TFullClient<Block, RuntimeApi, RuntimeExecutor>>,
		(),
	>,
	sc_service::Error,
> {
	let inherent_data_providers = sp_inherents::InherentDataProviders::new();

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, RuntimeExecutor>(&config, None)?;
	let client = Arc::new(client);

	let registry = config.prometheus_registry();

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_handle(),
		client.clone(),
	);

	let import_queue = cumulus_client_consensus_relay_chain::import_queue(
		client.clone(),
		client.clone(),
		inherent_data_providers.clone(),
		&task_manager.spawn_essential_handle(),
		registry.clone(),
	)?;

	let params = PartialComponents {
		backend,
		client,
		import_queue,
		keystore_container,
		task_manager,
		transaction_pool,
		inherent_data_providers,
		select_chain: (),
		other: (),
	};

	Ok(params)
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
#[sc_tracing::logging::prefix_logs_with(parachain_config.network.node_name.as_str())]
async fn start_node_impl<RB>(
	parachain_config: Configuration,
	collator_key: Option<CollatorPair>,
	relay_chain_config: Configuration,
	para_id: ParaId,
	rpc_ext_builder: RB,
) -> sc_service::error::Result<(
	TaskManager,
	Arc<TFullClient<Block, RuntimeApi, RuntimeExecutor>>,
	Arc<NetworkService<Block, H256>>,
	RpcHandlers,
)>
where
	RB: Fn(
			Arc<TFullClient<Block, RuntimeApi, RuntimeExecutor>>,
		) -> jsonrpc_core::IoHandler<sc_rpc::Metadata>
		+ Send
		+ 'static,
{
	if matches!(parachain_config.role, Role::Light) {
		return Err("Light client not supported!".into());
	}

	let mut parachain_config = prepare_node_config(parachain_config);

	let params = new_partial(&mut parachain_config)?;
	params
		.inherent_data_providers
		.register_provider(sp_timestamp::InherentDataProvider)
		.expect("Registers timestamp inherent data provider.");

	let transaction_pool = params.transaction_pool.clone();
	let mut task_manager = params.task_manager;

	let relay_chain_full_node = polkadot_test_service::new_full(
		relay_chain_config,
		if let Some(ref key) = collator_key {
			polkadot_service::IsCollator::Yes(key.clone())
		} else {
			polkadot_service::IsCollator::No
		},
		None,
	)
	.map_err(|e| match e {
		polkadot_service::Error::Sub(x) => x,
		s => format!("{}", s).into(),
	})?;

	let client = params.client.clone();
	let backend = params.backend.clone();
	let block_announce_validator = BlockAnnounceValidator::new(
		relay_chain_full_node.client.clone(),
		para_id,
		Box::new(relay_chain_full_node.network.clone()),
		relay_chain_full_node.backend.clone(),
		relay_chain_full_node.client.clone(),
	);
	let block_announce_validator_builder = move |_| Box::new(block_announce_validator) as Box<_>;

	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let import_queue = params.import_queue;
	let (network, network_status_sinks, system_rpc_tx, start_network) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &parachain_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: None,
			block_announce_validator_builder: Some(Box::new(block_announce_validator_builder)),
		})?;

	let rpc_extensions_builder = {
		let client = client.clone();

		Box::new(move |_, _| rpc_ext_builder(client.clone()))
	};

	let rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		on_demand: None,
		remote_blockchain: None,
		rpc_extensions_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: params.keystore_container.sync_keystore(),
		backend,
		network: network.clone(),
		network_status_sinks,
		system_rpc_tx,
		telemetry: None,
	})?;

	let announce_block = {
		let network = network.clone();
		Arc::new(move |hash, data| network.announce_block(hash, data))
	};

	if let Some(collator_key) = collator_key {
		let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool,
			prometheus_registry.as_ref(),
			None,
		);
		let parachain_consensus = cumulus_client_consensus_relay_chain::RelayChainConsensus::new(
			para_id,
			proposer_factory,
			params.inherent_data_providers,
			client.clone(),
			relay_chain_full_node.client.clone(),
			relay_chain_full_node.backend.clone(),
		);

		let relay_chain_full_node =
			relay_chain_full_node.with_client(polkadot_test_service::TestClient);

		let params = StartCollatorParams {
			backend: params.backend,
			block_status: client.clone(),
			announce_block,
			client: client.clone(),
			spawner: task_manager.spawn_handle(),
			task_manager: &mut task_manager,
			para_id,
			collator_key,
			parachain_consensus: Box::new(parachain_consensus),
			relay_chain_full_node,
		};

		start_collator(params).await?;
	} else {
		let relay_chain_full_node =
			relay_chain_full_node.with_client(polkadot_test_service::TestClient);

		let params = StartFullNodeParams {
			client: client.clone(),
			announce_block,
			task_manager: &mut task_manager,
			para_id,
			polkadot_full_node: relay_chain_full_node,
		};

		start_full_node(params)?;
	}

	start_network.start_network();

	Ok((task_manager, client, network, rpc_handlers))
}

/// A Cumulus test node instance used for testing.
pub struct TestNode {
	/// TaskManager's instance.
	pub task_manager: TaskManager,
	/// Client's instance.
	pub client: Arc<TFullClient<Block, RuntimeApi, RuntimeExecutor>>,
	/// Node's network.
	pub network: Arc<NetworkService<Block, H256>>,
	/// The `MultiaddrWithPeerId` to this node. This is useful if you want to pass it as "boot node"
	/// to other nodes.
	pub addr: MultiaddrWithPeerId,
	/// RPCHandlers to make RPC queries.
	pub rpc_handlers: RpcHandlers,
}


/// A builder to create a [`TestNode`].
pub struct TestNodeBuilder {
	para_id: ParaId,
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	collator_key: Option<CollatorPair>,
	parachain_nodes: Vec<MultiaddrWithPeerId>,
	parachain_nodes_exclusive: bool,
	relay_chain_nodes: Vec<MultiaddrWithPeerId>,
}

impl TestNodeBuilder {
	/// Create a new instance of `Self`.
	///
	/// `para_id` - The parachain id this node is running for.
	/// `task_executor` - The task executor to use.
	/// `key` - The key that will be used to generate the name and that will be passed as `dev_seed`.
	pub fn new(para_id: ParaId, task_executor: TaskExecutor, key: Sr25519Keyring) -> Self {
		TestNodeBuilder {
			key,
			para_id,
			task_executor,
			collator_key: None,
			parachain_nodes: Vec::new(),
			parachain_nodes_exclusive: false,
			relay_chain_nodes: Vec::new(),
		}
	}

	/// Enable collator for this node.
	pub fn enable_collator(mut self) -> Self {
		let collator_key = CollatorPair::generate().0;
		self.collator_key = Some(collator_key);
		self
	}

	/// Instruct the node to exclusively connect to registered parachain nodes.
	///
	/// Parachain nodes can be registered using [`Self::connect_to_parachain_node`] and
	/// [`Self::connect_to_parachain_nodes`].
	pub fn exclusively_connect_to_registered_parachain_nodes(mut self) -> Self {
		self.parachain_nodes_exclusive = true;
		self
	}

	/// Make the node connect to the given parachain node.
	///
	/// By default the node will not be connected to any node or will be able to discover any other
	/// node.
	pub fn connect_to_parachain_node(mut self, node: &TestNode) -> Self {
		self.parachain_nodes.push(node.addr.clone());
		self
	}

	/// Make the node connect to the given parachain nodes.
	///
	/// By default the node will not be connected to any node or will be able to discover any other
	/// node.
	pub fn connect_to_parachain_nodes<'a>(
		mut self,
		nodes: impl Iterator<Item = &'a TestNode>,
	) -> Self {
		self.parachain_nodes.extend(nodes.map(|n| n.addr.clone()));
		self
	}

	/// Make the node connect to the given relay chain node.
	///
	/// By default the node will not be connected to any node or will be able to discover any other
	/// node.
	pub fn connect_to_relay_chain_node(
		mut self,
		node: &polkadot_test_service::PolkadotTestNode,
	) -> Self {
		self.relay_chain_nodes.push(node.addr.clone());
		self
	}

	/// Make the node connect to the given relay chain nodes.
	///
	/// By default the node will not be connected to any node or will be able to discover any other
	/// node.
	pub fn connect_to_relay_chain_nodes<'a>(
		mut self,
		nodes: impl IntoIterator<Item = &'a polkadot_test_service::PolkadotTestNode>,
	) -> Self {
		self.relay_chain_nodes.extend(nodes.into_iter().map(|n| n.addr.clone()));
		self
	}

	/// Build the [`TestNode`].
	pub async fn build(self) -> TestNode {
		let parachain_config = node_config(
			|| (),
			self.task_executor.clone(),
			self.key.clone(),
			self.parachain_nodes,
			self.parachain_nodes_exclusive,
			self.para_id,
			self.collator_key.is_some(),
		)
		.expect("could not generate Configuration");
		let mut relay_chain_config = polkadot_test_service::node_config(
			|| (),
			self.task_executor,
			self.key,
			self.relay_chain_nodes,
			false,
		);

		relay_chain_config.network.node_name =
			format!("{} (relay chain)", relay_chain_config.network.node_name);

		let multiaddr = parachain_config.network.listen_addresses[0].clone();
		let (task_manager, client, network, rpc_handlers) = start_node_impl(
			parachain_config,
			self.collator_key,
			relay_chain_config,
			self.para_id,
			|_| Default::default(),
		)
		.await
		.expect("could not create Cumulus test service");

		let peer_id = network.local_peer_id().clone();
		let addr = MultiaddrWithPeerId { multiaddr, peer_id };

		TestNode {
			task_manager,
			client,
			network,
			addr,
			rpc_handlers,
		}
	}
}

/// Create a Cumulus `Configuration`.
///
/// By default an in-memory socket will be used, therefore you need to provide nodes if you want the
/// node to be connected to other nodes. If `nodes_exclusive` is `true`, the node will only connect
/// to the given `nodes` and not to any other node. The `storage_update_func` can be used to make
/// adjustments to the runtime genesis.
pub fn node_config(
	storage_update_func: impl Fn(),
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	nodes: Vec<MultiaddrWithPeerId>,
	nodes_exlusive: bool,
	para_id: ParaId,
	is_collator: bool,
) -> Result<Configuration, ServiceError> {
	let base_path = BasePath::new_temp_dir()?;
	let root = base_path.path().to_path_buf();
	let role = if is_collator {
		Role::Authority
	} else {
		Role::Full
	};
	let key_seed = key.to_seed();
	let mut spec = Box::new(chain_spec::get_chain_spec(para_id));

	let mut storage = spec
		.as_storage_builder()
		.build_storage()
		.expect("could not build storage");

	BasicExternalities::execute_with_storage(&mut storage, storage_update_func);
	spec.set_storage(storage);

	let mut network_config = NetworkConfiguration::new(
		format!("{} (parachain)", key_seed.to_string()),
		"network/test/0.1",
		Default::default(),
		None,
	);

	if nodes_exlusive {
		network_config.default_peers_set.reserved_nodes = nodes;
		network_config.default_peers_set.non_reserved_mode =
			sc_network::config::NonReservedPeerMode::Deny;
	} else {
		network_config.boot_nodes = nodes;
	}

	network_config.allow_non_globals_in_dht = true;

	network_config
		.listen_addresses
		.push(multiaddr::Protocol::Memory(rand::random()).into());

	network_config.transport = TransportConfig::MemoryOnly;

	Ok(Configuration {
		impl_name: "cumulus-test-node".to_string(),
		impl_version: "0.1".to_string(),
		role,
		task_executor,
		transaction_pool: Default::default(),
		network: network_config,
		keystore: KeystoreConfig::InMemory,
		keystore_remote: Default::default(),
		database: DatabaseConfig::RocksDb {
			path: root.join("db"),
			cache_size: 128,
		},
		state_cache_size: 67108864,
		state_cache_child_ratio: None,
		state_pruning: PruningMode::ArchiveAll,
		keep_blocks: KeepBlocks::All,
		transaction_storage: TransactionStorageMode::BlockBody,
		chain_spec: spec,
		wasm_method: WasmExecutionMethod::Interpreted,
		// NOTE: we enforce the use of the native runtime to make the errors more debuggable
		execution_strategies: ExecutionStrategies {
			syncing: sc_client_api::ExecutionStrategy::NativeWhenPossible,
			importing: sc_client_api::ExecutionStrategy::NativeWhenPossible,
			block_construction: sc_client_api::ExecutionStrategy::NativeWhenPossible,
			offchain_worker: sc_client_api::ExecutionStrategy::NativeWhenPossible,
			other: sc_client_api::ExecutionStrategy::NativeWhenPossible,
		},
		rpc_http: None,
		rpc_ws: None,
		rpc_ipc: None,
		rpc_ws_max_connections: None,
		rpc_cors: None,
		rpc_methods: Default::default(),
		prometheus_config: None,
		telemetry_endpoints: None,
		telemetry_external_transport: None,
		default_heap_pages: None,
		offchain_worker: OffchainWorkerConfig {
			enabled: true,
			indexing_enabled: false,
		},
		force_authoring: false,
		disable_grandpa: false,
		dev_key_seed: Some(key_seed),
		tracing_targets: None,
		tracing_receiver: Default::default(),
		max_runtime_instances: 8,
		announce_block: true,
		base_path: Some(base_path),
		informant_output_format: Default::default(),
		wasm_runtime_overrides: None,
		disable_log_reloading: false,
	})
}

impl TestNode {
	/// Wait for `count` blocks to be imported in the node and then exit. This function will not
	/// return if no blocks are ever created, thus you should restrict the maximum amount of time of
	/// the test execution.
	pub fn wait_for_blocks(&self, count: usize) -> impl Future<Output = ()> {
		self.client.wait_for_blocks(count)
	}
}

/// Run a relay-chain validator node.
///
/// This is essentially a wrapper around
/// [`run_validator_node`](polkadot_test_service::run_validator_node).
pub fn run_relay_chain_validator_node(
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	storage_update_func: impl Fn(),
	boot_nodes: Vec<MultiaddrWithPeerId>,
) -> polkadot_test_service::PolkadotTestNode {
	polkadot_test_service::run_validator_node(
		task_executor,
		key,
		storage_update_func,
		boot_nodes,
		Some(cumulus_test_relay_validation_worker_provider::VALIDATION_WORKER.into()),
	)
}
