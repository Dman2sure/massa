// Copyright (c) 2022 MASSA LABS <info@massa.net>
#![allow(clippy::too_many_arguments)]
use crate::error::ApiError;
use crate::settings::APISettings;
use crate::{Endpoints, Public, RpcServer, StopHandle, API};
use futures::{stream::FuturesUnordered, StreamExt};
use jsonrpc_core::BoxFuture;
use massa_consensus_exports::{ConsensusCommandSender, ConsensusConfig};
use massa_execution_exports::{
    ExecutionController, ExecutionStackElement, ReadOnlyExecutionRequest, ReadOnlyExecutionTarget,
};
use massa_graph::{DiscardReason, ExportBlockStatus};
use massa_models::api::{
    DatastoreEntryInput, DatastoreEntryOutput, OperationInput, ReadOnlyBytecodeExecution,
    ReadOnlyCall,
};
use massa_models::execution::ReadOnlyResult;
use massa_models::operation::OperationDeserializer;
use massa_models::wrapped::WrappedDeserializer;
use massa_models::{Amount, ModelsError, WrappedOperation};
use massa_pos_exports::SelectorController;
use massa_serialization::{DeserializeError, Deserializer};

use massa_models::{
    api::{
        AddressInfo, BlockInfo, BlockInfoContent, BlockSummary, EndorsementInfo, EventFilter,
        NodeStatus, OperationInfo, TimeInterval,
    },
    clique::Clique,
    composite::PubkeySig,
    execution::ExecuteReadOnlyResponse,
    node::NodeId,
    output_event::SCOutputEvent,
    prehash::{BuildMap, Map, Set},
    timeslots::{get_latest_block_slot_at_timestamp, time_range_to_slot_range},
    Address, BlockId, CompactConfig, EndorsementId, OperationId, Slot, Version,
};
use massa_network_exports::{NetworkCommandSender, NetworkSettings};
use massa_pool::PoolCommandSender;
use massa_signature::KeyPair;
use massa_time::MassaTime;
use std::collections::{BTreeSet, HashSet};
use std::net::{IpAddr, SocketAddr};

impl API<Public> {
    /// generate a new public API
    pub fn new(
        consensus_command_sender: ConsensusCommandSender,
        execution_controller: Box<dyn ExecutionController>,
        selector_controller: Box<dyn SelectorController>,
        api_settings: &'static APISettings,
        consensus_settings: ConsensusConfig,
        pool_command_sender: PoolCommandSender,
        network_settings: &'static NetworkSettings,
        version: Version,
        network_command_sender: NetworkCommandSender,
        compensation_millis: i64,
        node_id: NodeId,
    ) -> Self {
        API(Public {
            consensus_command_sender,
            consensus_config: consensus_settings,
            api_settings,
            pool_command_sender,
            network_settings,
            version,
            network_command_sender,
            compensation_millis,
            node_id,
            execution_controller,
            selector_controller,
        })
    }
}

impl RpcServer for API<Public> {
    fn serve(self, url: &SocketAddr) -> StopHandle {
        crate::serve(self, url)
    }
}

#[doc(hidden)]
impl Endpoints for API<Public> {
    fn stop_node(&self) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn node_sign_message(&self, _: Vec<u8>) -> BoxFuture<Result<PubkeySig, ApiError>> {
        crate::wrong_api::<PubkeySig>()
    }

    fn add_staking_secret_keys(&self, _: Vec<KeyPair>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn execute_read_only_bytecode(
        &self,
        reqs: Vec<ReadOnlyBytecodeExecution>,
    ) -> BoxFuture<Result<Vec<ExecuteReadOnlyResponse>, ApiError>> {
        if reqs.len() as u64 > self.0.api_settings.max_arguments {
            let closure =
                async move || Err(ApiError::TooManyArguments("too many arguments".into()));
            return Box::pin(closure());
        }

        let mut res: Vec<ExecuteReadOnlyResponse> = Vec::with_capacity(reqs.len());
        for ReadOnlyBytecodeExecution {
            max_gas,
            address,
            simulated_gas_price,
            bytecode,
        } in reqs
        {
            let address = address.unwrap_or_else(|| {
                // if no addr provided, use a random one
                Address::from_public_key(&KeyPair::generate().get_public_key())
            });

            // TODO:
            // * set a maximum gas value for read-only executions to prevent attacks
            // * stop mapping request and result, reuse execution's structures
            // * remove async stuff

            // translate request
            let req = ReadOnlyExecutionRequest {
                max_gas,
                simulated_gas_price,
                target: ReadOnlyExecutionTarget::BytecodeExecution(bytecode),
                call_stack: vec![ExecutionStackElement {
                    address,
                    coins: Default::default(),
                    owned_addresses: vec![address],
                }],
            };

            // run
            let result = self.0.execution_controller.execute_readonly_request(req);

            // map result
            let result = ExecuteReadOnlyResponse {
                executed_at: result.as_ref().map_or_else(|_| Slot::new(0, 0), |v| v.slot),
                result: result.as_ref().map_or_else(
                    |err| ReadOnlyResult::Error(format!("readonly call failed: {}", err)),
                    |_| ReadOnlyResult::Ok,
                ),
                output_events: result.map_or_else(|_| Default::default(), |mut v| v.events.take()),
            };

            res.push(result);
        }

        // return result
        let closure = async move || Ok(res);
        Box::pin(closure())
    }

    fn execute_read_only_call(
        &self,
        reqs: Vec<ReadOnlyCall>,
    ) -> BoxFuture<Result<Vec<ExecuteReadOnlyResponse>, ApiError>> {
        if reqs.len() as u64 > self.0.api_settings.max_arguments {
            let closure =
                async move || Err(ApiError::TooManyArguments("too many arguments".into()));
            return Box::pin(closure());
        }

        let mut res: Vec<ExecuteReadOnlyResponse> = Vec::with_capacity(reqs.len());
        for ReadOnlyCall {
            max_gas,
            simulated_gas_price,
            target_address,
            target_function,
            parameter,
            caller_address,
        } in reqs
        {
            let caller_address = caller_address.unwrap_or_else(|| {
                // if no addr provided, use a random one
                Address::from_public_key(&KeyPair::generate().get_public_key())
            });

            // TODO:
            // * set a maximum gas value for read-only executions to prevent attacks
            // * stop mapping request and result, reuse execution's structures
            // * remove async stuff

            // translate request
            let req = ReadOnlyExecutionRequest {
                max_gas,
                simulated_gas_price,
                target: ReadOnlyExecutionTarget::FunctionCall {
                    target_func: target_function,
                    target_addr: target_address,
                    parameter,
                },
                call_stack: vec![
                    ExecutionStackElement {
                        address: caller_address,
                        coins: Default::default(),
                        owned_addresses: vec![caller_address],
                    },
                    ExecutionStackElement {
                        address: target_address,
                        coins: Default::default(),
                        owned_addresses: vec![target_address],
                    },
                ],
            };

            // run
            let result = self.0.execution_controller.execute_readonly_request(req);

            // map result
            let result = ExecuteReadOnlyResponse {
                executed_at: result.as_ref().map_or_else(|_| Slot::new(0, 0), |v| v.slot),
                result: result.as_ref().map_or_else(
                    |err| ReadOnlyResult::Error(format!("readonly call failed: {}", err)),
                    |_| ReadOnlyResult::Ok,
                ),
                output_events: result.map_or_else(|_| Default::default(), |mut v| v.events.take()),
            };

            res.push(result);
        }

        // return result
        let closure = async move || Ok(res);
        Box::pin(closure())
    }

    fn remove_staking_addresses(&self, _: Vec<Address>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn get_staking_addresses(&self) -> BoxFuture<Result<Set<Address>, ApiError>> {
        crate::wrong_api::<Set<Address>>()
    }

    fn node_ban_by_ip(&self, _: Vec<IpAddr>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn node_ban_by_id(&self, _: Vec<NodeId>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn node_unban_by_ip(&self, _: Vec<IpAddr>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn node_unban_by_id(&self, _: Vec<NodeId>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn get_status(&self) -> BoxFuture<Result<NodeStatus, ApiError>> {
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let network_command_sender = self.0.network_command_sender.clone();
        let network_config = self.0.network_settings.clone();
        let version = self.0.version;
        let consensus_settings = self.0.consensus_config.clone();
        let compensation_millis = self.0.compensation_millis;
        let mut pool_command_sender = self.0.pool_command_sender.clone();
        let node_id = self.0.node_id;
        let config = CompactConfig::default();
        let closure = async move || {
            let now = MassaTime::compensated_now(compensation_millis)?;
            let last_slot = get_latest_block_slot_at_timestamp(
                consensus_settings.thread_count,
                consensus_settings.t0,
                consensus_settings.genesis_timestamp,
                now,
            )?;

            let (consensus_stats, network_stats, pool_stats, peers) = tokio::join!(
                consensus_command_sender.get_stats(),
                network_command_sender.get_network_stats(),
                pool_command_sender.get_pool_stats(),
                network_command_sender.get_peers()
            );
            Ok(NodeStatus {
                node_id,
                node_ip: network_config.routable_ip,
                version,
                current_time: now,
                connected_nodes: peers?
                    .peers
                    .iter()
                    .flat_map(|(ip, peer)| {
                        peer.active_nodes
                            .iter()
                            .map(move |(id, is_outgoing)| (*id, (*ip, *is_outgoing)))
                    })
                    .collect(),
                last_slot,
                next_slot: last_slot
                    .unwrap_or_else(|| Slot::new(0, 0))
                    .get_next_slot(consensus_settings.thread_count)?,
                consensus_stats: consensus_stats?,
                network_stats: network_stats?,
                pool_stats: pool_stats?,

                config,
                current_cycle: last_slot
                    .unwrap_or_else(|| Slot::new(0, 0))
                    .get_cycle(consensus_settings.periods_per_cycle),
            })
        };
        Box::pin(closure())
    }

    fn get_cliques(&self) -> BoxFuture<Result<Vec<Clique>, ApiError>> {
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let closure = async move || Ok(consensus_command_sender.get_cliques().await?);
        Box::pin(closure())
    }

    fn get_stakers(&self) -> BoxFuture<Result<Vec<(Address, u64)>, ApiError>> {
        let execution_controller = self.0.execution_controller.clone();
        let cfg = self.0.consensus_config.clone();
        let compensation_millis = self.0.compensation_millis;

        let closure = async move || {
            let curr_cycle = get_latest_block_slot_at_timestamp(
                cfg.thread_count,
                cfg.t0,
                cfg.genesis_timestamp,
                MassaTime::compensated_now(compensation_millis)?,
            )?
            .unwrap_or_else(|| Slot::new(0, 0))
            .get_cycle(cfg.periods_per_cycle);
            let mut staker_vec = execution_controller
                .get_cycle_rolls(curr_cycle)
                .into_iter()
                .collect::<Vec<(Address, u64)>>();
            staker_vec.sort_by_key(|(_, rolls)| *rolls);
            Ok(staker_vec)
        };
        Box::pin(closure())
    }

    fn get_operations(
        &self,
        ops: Vec<OperationId>,
    ) -> BoxFuture<Result<Vec<OperationInfo>, ApiError>> {
        let api_cfg = self.0.api_settings;
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let mut pool_command_sender = self.0.pool_command_sender.clone();
        let closure = async move || {
            if ops.len() as u64 > api_cfg.max_arguments {
                return Err(ApiError::TooManyArguments("too many arguments".into()));
            }

            let operation_ids: Set<OperationId> = ops.iter().cloned().collect();

            // simultaneously ask pool and consensus
            let (pool_res, consensus_res) = tokio::join!(
                pool_command_sender.get_operations(operation_ids.clone()),
                consensus_command_sender.get_operations(operation_ids)
            );
            let (pool_res, consensus_res) = (pool_res?, consensus_res?);
            let mut res: Map<OperationId, OperationInfo> = Map::with_capacity_and_hasher(
                pool_res.len() + consensus_res.len(),
                BuildMap::default(),
            );

            // add pool info
            res.extend(pool_res.into_iter().map(|(id, operation)| {
                (
                    id,
                    OperationInfo {
                        operation,
                        in_pool: true,
                        in_blocks: Vec::new(),
                        id,
                        is_final: false,
                    },
                )
            }));

            // add consensus info
            consensus_res.into_iter().for_each(|(op_id, search_new)| {
                let search_new = OperationInfo {
                    id: op_id,
                    in_pool: search_new.in_pool,
                    in_blocks: search_new.in_blocks.keys().copied().collect(),
                    is_final: search_new
                        .in_blocks
                        .iter()
                        .any(|(_, (_, is_final))| *is_final),
                    operation: search_new.op,
                };
                res.entry(op_id)
                    .and_modify(|search_old| search_old.extend(&search_new))
                    .or_insert(search_new);
            });

            // return values in the right order
            Ok(ops
                .into_iter()
                .filter_map(|op_id| res.remove(&op_id))
                .collect())
        };
        Box::pin(closure())
    }

    fn get_endorsements(
        &self,
        eds: Vec<EndorsementId>,
    ) -> BoxFuture<Result<Vec<EndorsementInfo>, ApiError>> {
        let api_cfg = self.0.api_settings;
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let pool_command_sender = self.0.pool_command_sender.clone();
        let closure = async move || {
            if eds.len() as u64 > api_cfg.max_arguments {
                return Err(ApiError::TooManyArguments("too many arguments".into()));
            }
            let mapped: Set<EndorsementId> = eds.into_iter().collect();
            let mut res = consensus_command_sender
                .get_endorsements_by_id(mapped.clone())
                .await?;
            for (id, endorsement) in pool_command_sender.get_endorsements_by_id(mapped).await? {
                res.entry(id)
                    .and_modify(|EndorsementInfo { in_pool, .. }| *in_pool = true)
                    .or_insert(EndorsementInfo {
                        id,
                        in_pool: true,
                        in_blocks: vec![],
                        is_final: false,
                        endorsement,
                    });
            }
            Ok(res.values().cloned().collect())
        };
        Box::pin(closure())
    }

    /// gets a block. Returns None if not found
    /// only active blocks are returned
    fn get_block(&self, id: BlockId) -> BoxFuture<Result<BlockInfo, ApiError>> {
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let closure = async move || {
            let cliques = consensus_command_sender.get_cliques().await?;
            let blockclique = cliques
                .iter()
                .find(|clique| clique.is_blockclique)
                .ok_or_else(|| ApiError::InconsistencyError("Missing block clique".to_string()))?;

            if let Some((block, is_final)) =
                match consensus_command_sender.get_block_status(id).await? {
                    Some(ExportBlockStatus::Active(block)) => Some((block, false)),
                    Some(ExportBlockStatus::Incoming) => None,
                    Some(ExportBlockStatus::WaitingForSlot) => None,
                    Some(ExportBlockStatus::WaitingForDependencies) => None,
                    Some(ExportBlockStatus::Discarded(_)) => None, // TODO: get block if stale
                    Some(ExportBlockStatus::Final(block)) => Some((block, true)),
                    None => None,
                }
            {
                Ok(BlockInfo {
                    id,
                    content: Some(BlockInfoContent {
                        is_final,
                        is_stale: false,
                        is_in_blockclique: blockclique.block_ids.contains(&id),
                        block,
                    }),
                })
            } else {
                Ok(BlockInfo { id, content: None })
            }
        };
        Box::pin(closure())
    }

    /// gets an interval of the block graph from consensus, with time filtering
    /// time filtering is done consensus-side to prevent communication overhead
    fn get_graph_interval(
        &self,
        time: TimeInterval,
    ) -> BoxFuture<Result<Vec<BlockSummary>, ApiError>> {
        let consensus_command_sender = self.0.consensus_command_sender.clone();
        let consensus_settings = self.0.consensus_config.clone();
        let closure = async move || {
            // filter blocks from graph_export
            let (start_slot, end_slot) = time_range_to_slot_range(
                consensus_settings.thread_count,
                consensus_settings.t0,
                consensus_settings.genesis_timestamp,
                time.start,
                time.end,
            )?;
            let graph = consensus_command_sender
                .get_block_graph_status(start_slot, end_slot)
                .await?;
            let mut res = Vec::with_capacity(graph.active_blocks.len());
            let blockclique = graph
                .max_cliques
                .iter()
                .find(|clique| clique.is_blockclique)
                .ok_or_else(|| ApiError::InconsistencyError("missing blockclique".to_string()))?;
            for (id, exported_block) in graph.active_blocks.into_iter() {
                res.push(BlockSummary {
                    id,
                    is_final: exported_block.is_final,
                    is_stale: false,
                    is_in_blockclique: blockclique.block_ids.contains(&id),
                    slot: exported_block.header.content.slot,
                    creator: exported_block.header.creator_address,
                    parents: exported_block.header.content.parents,
                });
            }
            for (id, (reason, header)) in graph.discarded_blocks.into_iter() {
                if reason == DiscardReason::Stale {
                    res.push(BlockSummary {
                        id,
                        is_final: false,
                        is_stale: true,
                        is_in_blockclique: false,
                        slot: header.content.slot,
                        creator: header.creator_address,
                        parents: header.content.parents,
                    });
                }
            }
            Ok(res)
        };
        Box::pin(closure())
    }

    fn get_datastore_entries(
        &self,
        entries: Vec<DatastoreEntryInput>,
    ) -> BoxFuture<Result<Vec<DatastoreEntryOutput>, ApiError>> {
        let execution_controller = self.0.execution_controller.clone();
        let closure = async move || {
            Ok(execution_controller
                .get_final_and_active_data_entry(
                    entries
                        .into_iter()
                        .map(|input| (input.address, input.key))
                        .collect::<Vec<_>>(),
                )
                .into_iter()
                .map(|output| DatastoreEntryOutput {
                    final_value: output.0,
                    candidate_value: output.1,
                })
                .collect())
        };
        Box::pin(closure())
    }

    fn get_addresses(
        &self,
        addresses: Vec<Address>,
    ) -> BoxFuture<Result<Vec<AddressInfo>, ApiError>> {
        let cmd_sender = self.0.consensus_command_sender.clone();
        let cfg = self.0.consensus_config.clone();
        let api_cfg = self.0.api_settings;
        let pool_command_sender = self.0.pool_command_sender.clone();
        let execution_controller = self.0.execution_controller.clone();
        let compensation_millis = self.0.compensation_millis.clone();
        let selector_controller = self.0.selector_controller.clone();

        let closure = async move || {
            let mut res = Vec::with_capacity(addresses.len());

            // check for address length
            if addresses.len() as u64 > api_cfg.max_arguments {
                return Err(ApiError::TooManyArguments("too many arguments".into()));
            }

            // roll and balance info
            let mut states = cmd_sender
                .get_addresses_info(addresses.iter().copied().collect())
                .await?;

            // operations block and endorsement info
            let mut operations: Map<Address, Set<OperationId>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut blocks: Map<Address, Set<BlockId>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut endorsements: Map<Address, Set<EndorsementId>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut final_balance_info: Map<Address, Option<Amount>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut candidate_balance_info: Map<Address, Option<Amount>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut final_datastore_keys: Map<Address, BTreeSet<Vec<u8>>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());
            let mut candidate_datastore_keys: Map<Address, BTreeSet<Vec<u8>>> =
                Map::with_capacity_and_hasher(addresses.len(), BuildMap::default());

            let mut concurrent_getters = FuturesUnordered::new();
            for &address in addresses.iter() {
                let mut pool_cmd_snd = pool_command_sender.clone();
                let cmd_snd = cmd_sender.clone();
                let exec_snd = execution_controller.clone();
                concurrent_getters.push(async move {
                    let blocks = cmd_snd
                        .get_block_ids_by_creator(address)
                        .await?
                        .into_keys()
                        .collect::<Set<BlockId>>();

                    let get_pool_ops = pool_cmd_snd.get_operations_involving_address(address);
                    let get_consensus_ops = cmd_snd.get_operations_involving_address(address);
                    let (get_pool_ops, get_consensus_ops) =
                        tokio::join!(get_pool_ops, get_consensus_ops);
                    let gathered: Set<OperationId> = get_pool_ops?
                        .into_keys()
                        .chain(get_consensus_ops?.into_keys())
                        .collect();

                    let get_pool_eds = pool_cmd_snd.get_endorsements_by_address(address);
                    let get_consensus_eds = cmd_snd.get_endorsements_by_address(address);
                    let (get_pool_eds, get_consensus_eds) =
                        tokio::join!(get_pool_eds, get_consensus_eds);
                    let gathered_ed: Set<EndorsementId> = get_pool_eds?
                        .into_keys()
                        .chain(get_consensus_eds?.into_keys())
                        .collect();

                    let balances = exec_snd.get_final_and_active_parallel_balance(vec![address]);
                    let balances_result = balances.first().unwrap();
                    let (final_keys, candidate_keys) =
                        exec_snd.get_final_and_active_datastore_keys(&address);

                    Result::<
                        (
                            Address,
                            Set<BlockId>,
                            Set<OperationId>,
                            Set<EndorsementId>,
                            Option<Amount>,
                            Option<Amount>,
                            BTreeSet<Vec<u8>>,
                            BTreeSet<Vec<u8>>,
                        ),
                        ApiError,
                    >::Ok((
                        address,
                        blocks,
                        gathered,
                        gathered_ed,
                        balances_result.0,
                        balances_result.1,
                        final_keys,
                        candidate_keys,
                    ))
                });
            }
            while let Some(res) = concurrent_getters.next().await {
                let (
                    addr,
                    block_set,
                    operation_set,
                    endorsement_set,
                    final_balance,
                    candidate_balance,
                    final_keys,
                    candidate_keys,
                ) = res?;
                blocks.insert(addr, block_set);
                operations.insert(addr, operation_set);
                endorsements.insert(addr, endorsement_set);
                final_balance_info.insert(addr, final_balance);
                candidate_balance_info.insert(addr, candidate_balance);
                final_datastore_keys.insert(addr, final_keys);
                candidate_datastore_keys.insert(addr, candidate_keys);
            }

            let curr_slot = get_latest_block_slot_at_timestamp(
                cfg.thread_count,
                cfg.t0,
                cfg.genesis_timestamp,
                MassaTime::compensated_now(compensation_millis)?,
            )?
            .unwrap_or_else(|| Slot::new(0, 0));

            let end_slot = Slot::new(
                curr_slot.period + api_cfg.draw_lookahead_period_count,
                curr_slot.thread,
            );

            // compile everything per address
            for address in addresses.into_iter() {
                let state = states.remove(&address).ok_or(ApiError::NotFound)?;
                let (block_draws, endorsement_draws) =
                    selector_controller.get_address_selections(&address, curr_slot, end_slot);

                res.push(AddressInfo {
                    address,
                    thread: address.get_thread(cfg.thread_count),
                    ledger_info: state.ledger_info,
                    rolls: state.rolls,
                    block_draws: HashSet::from_iter(block_draws.into_iter()),
                    endorsement_draws: HashSet::from_iter(endorsement_draws.into_iter()),
                    blocks_created: blocks.remove(&address).ok_or(ApiError::NotFound)?,
                    involved_in_endorsements: endorsements
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                    involved_in_operations: operations
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                    production_stats: state.production_stats,
                    final_balance_info: final_balance_info
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                    candidate_balance_info: candidate_balance_info
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                    final_datastore_keys: final_datastore_keys
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                    candidate_datastore_keys: candidate_datastore_keys
                        .remove(&address)
                        .ok_or(ApiError::NotFound)?,
                })
            }
            Ok(res)
        };
        Box::pin(closure())
    }

    fn send_operations(
        &self,
        ops: Vec<OperationInput>,
    ) -> BoxFuture<Result<Vec<OperationId>, ApiError>> {
        let mut cmd_sender = self.0.pool_command_sender.clone();
        let api_cfg = self.0.api_settings;
        let closure = async move || {
            if ops.len() as u64 > api_cfg.max_arguments {
                return Err(ApiError::TooManyArguments("too many arguments".into()));
            }
            let operation_deserializer = WrappedDeserializer::new(OperationDeserializer::new());
            let to_send = ops
                .into_iter()
                .map(|op_input| {
                    let mut op_serialized = Vec::new();
                    op_serialized.extend(op_input.signature.to_bytes());
                    op_serialized.extend(op_input.creator_public_key.to_bytes());
                    op_serialized.extend(op_input.serialized_content);
                    let (rest, op): (&[u8], WrappedOperation) = operation_deserializer
                        .deserialize::<DeserializeError>(&op_serialized)
                        .map_err(|err| {
                            ApiError::ModelsError(ModelsError::DeserializeError(err.to_string()))
                        })?;
                    if rest.is_empty() {
                        Ok(op)
                    } else {
                        Err(ApiError::ModelsError(ModelsError::DeserializeError(
                            "There is data left after operation deserialization".to_owned(),
                        )))
                    }
                })
                .map(|op| match op {
                    Ok(operation) => Ok((operation.verify_integrity()?, operation)),
                    Err(e) => Err(e),
                })
                .collect::<Result<Map<OperationId, _>, ApiError>>()?;
            let ids = to_send.keys().copied().collect();
            cmd_sender.add_operations(to_send).await?;
            Ok(ids)
        };
        Box::pin(closure())
    }

    /// Get events optionally filtered by:
    /// * start slot
    /// * end slot
    /// * emitter address
    /// * original caller address
    /// * operation id
    fn get_filtered_sc_output_event(
        &self,
        filter: EventFilter,
    ) -> BoxFuture<Result<Vec<SCOutputEvent>, ApiError>> {
        let events = self
            .0
            .execution_controller
            .get_filtered_sc_output_event(filter);

        // TODO: get rid of the async part
        let closure = async move || Ok(events);
        Box::pin(closure())
    }

    fn node_whitelist(&self, _: Vec<IpAddr>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }

    fn node_remove_from_whitelist(&self, _: Vec<IpAddr>) -> BoxFuture<Result<(), ApiError>> {
        crate::wrong_api::<()>()
    }
}
