// Copyright 2022 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use cumulus_relay_chain_interface::RelayChainError;
use lru::LruCache;
use polkadot_node_network_protocol::{
	peer_set::PeerSetProtocolNames,
	request_response::{
		v1::{AvailableDataFetchingRequest, CollationFetchingRequest},
		IncomingRequestReceiver, ReqProtocolNames,
	},
};
use polkadot_node_subsystem_util::metrics::{prometheus::Registry, Metrics};
use polkadot_overseer::{
	BlockInfo, DummySubsystem, MetricsTrait, Overseer, OverseerHandle, OverseerMetrics, SpawnGlue,
	KNOWN_LEAVES_CACHE_SIZE,
};
use polkadot_primitives::v2::CollatorPair;
use polkadot_service::{
	overseer::{
		AvailabilityRecoverySubsystem, CollationGenerationSubsystem, CollatorProtocolSubsystem,
		NetworkBridgeMetrics, NetworkBridgeRxSubsystem, NetworkBridgeTxSubsystem, ProtocolSide,
		RuntimeApiSubsystem,
	},
	Error, OverseerConnector,
};
use sc_authority_discovery::Service as AuthorityDiscoveryService;
use sc_network::NetworkStateInfo;

use std::sync::Arc;

use cumulus_primitives_core::relay_chain::{Block, Hash as PHash};

use polkadot_service::{Handle, TaskManager};

use crate::BlockChainRpcClient;
use futures::{select, StreamExt};
use sp_runtime::traits::Block as BlockT;

/// Arguments passed for overseer construction.
pub(crate) struct CollatorOverseerGenArgs<'a> {
	/// Runtime client generic, providing the `ProvieRuntimeApi` trait besides others.
	pub runtime_client: Arc<BlockChainRpcClient>,
	/// Underlying network service implementation.
	pub network_service: Arc<sc_network::NetworkService<Block, PHash>>,
	/// Underlying authority discovery service.
	pub authority_discovery_service: AuthorityDiscoveryService,
	/// Receiver for collation request protocol
	pub collation_req_receiver: IncomingRequestReceiver<CollationFetchingRequest>,
	/// Receiver for availability request protocol
	pub available_data_req_receiver: IncomingRequestReceiver<AvailableDataFetchingRequest>,
	/// Prometheus registry, commonly used for production systems, less so for test.
	pub registry: Option<&'a Registry>,
	/// Task spawner to be used throughout the overseer and the APIs it provides.
	pub spawner: sc_service::SpawnTaskHandle,
	/// Determines the behavior of the collator.
	pub collator_pair: CollatorPair,
	/// Request response protocols
	pub req_protocol_names: ReqProtocolNames,
	/// Peerset protocols name mapping
	pub peer_set_protocol_names: PeerSetProtocolNames,
}

fn build_overseer<'a>(
	connector: OverseerConnector,
	CollatorOverseerGenArgs {
		runtime_client,
		network_service,
		authority_discovery_service,
		collation_req_receiver,
		available_data_req_receiver,
		registry,
		spawner,
		collator_pair,
		req_protocol_names,
		peer_set_protocol_names,
	}: CollatorOverseerGenArgs<'a>,
) -> Result<
	(Overseer<SpawnGlue<sc_service::SpawnTaskHandle>, Arc<BlockChainRpcClient>>, OverseerHandle),
	Error,
> {
	let leaves = Vec::new();
	let metrics = <OverseerMetrics as MetricsTrait>::register(registry)?;
	let spawner = SpawnGlue(spawner);
	let network_bridge_metrics: NetworkBridgeMetrics = Metrics::register(registry)?;
	let builder = Overseer::builder()
		.availability_distribution(DummySubsystem)
		.availability_recovery(AvailabilityRecoverySubsystem::with_chunks_only(
			available_data_req_receiver,
			Metrics::register(registry)?,
		))
		.availability_store(DummySubsystem)
		.bitfield_distribution(DummySubsystem)
		.bitfield_signing(DummySubsystem)
		.candidate_backing(DummySubsystem)
		.candidate_validation(DummySubsystem)
		.pvf_checker(DummySubsystem)
		.chain_api(DummySubsystem)
		.collation_generation(CollationGenerationSubsystem::new(Metrics::register(registry)?))
		.collator_protocol({
			let side = ProtocolSide::Collator(
				network_service.local_peer_id().clone(),
				collator_pair,
				collation_req_receiver,
				Metrics::register(registry)?,
			);
			CollatorProtocolSubsystem::new(side)
		})
		.network_bridge_rx(NetworkBridgeRxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			Box::new(network_service.clone()),
			network_bridge_metrics.clone(),
			peer_set_protocol_names.clone(),
		))
		.network_bridge_tx(NetworkBridgeTxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			network_bridge_metrics,
			req_protocol_names,
			peer_set_protocol_names,
		))
		.provisioner(DummySubsystem)
		.runtime_api(RuntimeApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		))
		.statement_distribution(DummySubsystem)
		.approval_distribution(DummySubsystem)
		.approval_voting(DummySubsystem)
		.gossip_support(DummySubsystem)
		.dispute_coordinator(DummySubsystem)
		.dispute_distribution(DummySubsystem)
		.chain_selection(DummySubsystem)
		.leaves(Vec::from_iter(
			leaves
				.into_iter()
				.map(|BlockInfo { hash, parent_hash: _, number }| (hash, number)),
		))
		.activation_external_listeners(Default::default())
		.span_per_active_leaf(Default::default())
		.active_leaves(Default::default())
		.supports_parachains(runtime_client)
		.known_leaves(LruCache::new(KNOWN_LEAVES_CACHE_SIZE))
		.metrics(metrics)
		.spawner(spawner);

	builder.build_with_connector(connector).map_err(|e| e.into())
}

pub(crate) fn spawn_overseer(
	overseer_args: CollatorOverseerGenArgs,
	task_manager: &TaskManager,
	relay_chain_rpc_client: Arc<BlockChainRpcClient>,
) -> Result<polkadot_overseer::Handle, polkadot_service::Error> {
	let (overseer, overseer_handle) = build_overseer(OverseerConnector::default(), overseer_args)
		.map_err(|e| {
		tracing::error!("Failed to initialize overseer: {}", e);
		e
	})?;

	let overseer_handle = Handle::new(overseer_handle.clone());
	{
		let handle = overseer_handle.clone();
		task_manager.spawn_essential_handle().spawn_blocking(
			"overseer",
			None,
			Box::pin(async move {
				use futures::{pin_mut, FutureExt};

				let forward = forward_collator_events(relay_chain_rpc_client, handle).fuse();

				let overseer_fut = overseer.run().fuse();

				pin_mut!(overseer_fut);
				pin_mut!(forward);

				select! {
					_ = forward => (),
					_ = overseer_fut => (),
				}
			}),
		);
	}
	Ok(overseer_handle)
}

/// Minimal relay chain node representation
pub struct NewMinimalNode {
	/// Task manager running all tasks for the minimal node
	pub task_manager: TaskManager,
	/// Overseer handle to interact with subsystems
	pub overseer_handle: Handle,
	/// Network service
	pub network: Arc<sc_network::NetworkService<Block, <Block as BlockT>::Hash>>,
}

/// Glues together the [`Overseer`] and `BlockchainEvents` by forwarding
/// import and finality notifications into the [`OverseerHandle`].
async fn forward_collator_events(
	client: Arc<BlockChainRpcClient>,
	mut handle: Handle,
) -> Result<(), RelayChainError> {
	let mut finality = client.finality_notification_stream().await?.fuse();
	let mut imports = client.import_notification_stream().await?.fuse();

	loop {
		select! {
			f = finality.next() => {
				match f {
					Some(header) => {
						tracing::info!(target: "minimal-polkadot-node", "Received finalized block via RPC: #{} ({})", header.number, header.hash());
		let block_info = BlockInfo { hash: header.hash(), parent_hash: header.parent_hash, number: header.number };
						handle.block_finalized(block_info).await;
					}
					None => return Err(RelayChainError::GenericError("Relay chain finality stream ended.".to_string())),
				}
			},
			i = imports.next() => {
				match i {
					Some(header) => {
						tracing::info!(target: "minimal-polkadot-node", "Received imported block via RPC: #{} ({})", header.number, header.hash());
		let block_info = BlockInfo { hash: header.hash(), parent_hash: header.parent_hash, number: header.number };
						handle.block_imported(block_info).await;
					}
					None => return Err(RelayChainError::GenericError("Relay chain import stream ended.".to_string())),
				}
			}
		}
	}
}
