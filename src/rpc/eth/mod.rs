mod error;

use self::error::{
    EthRpcError, OutOfTimeRangeData, PaymasterValidationRejectedData, StakeTooLowData,
    ThrottledOrBannedData,
};
use super::{
    GasEstimate, RichUserOperation, RpcUserOperation, UserOperationOptionalGas,
    UserOperationReceipt,
};
use crate::common::{
    context::LogWithContext,
    contracts::entry_point::{
        EntryPoint, EntryPointCalls, EntryPointErrors, UserOperationEventFilter,
        UserOperationRevertReasonFilter,
    },
    eth::{get_revert_data, log_to_raw_log},
    protos::{
        op_pool::{
            op_pool_client::OpPoolClient, AddOpRequest, Entity as OpPoolEntity, ErrorInfo,
            ErrorReason, MempoolOp, StorageSlot,
        },
        to_le_bytes,
    },
    simulation::{self, SimulationError, SimulationSuccess, Simulator, SimulatorImpl, Violation},
    types::{Entity, Timestamp, UserOperation},
};
use anyhow::{anyhow, bail, Context};
use ethers::{
    abi::{AbiDecode, RawLog},
    prelude::{ContractError, EthEvent},
    providers::{Http, Middleware, Provider, ProviderError},
    types::{
        transaction::eip2718::TypedTransaction, Address, Bytes, Eip1559TransactionRequest, Filter,
        Log, OpCode, TransactionReceipt, H256, U256, U64,
    },
    utils::to_checksum,
};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use prost::Message;
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use tokio::join;
use tonic::{async_trait, transport::Channel, Status};
use tracing::{debug, Level};

/// Eth API
#[rpc(server, namespace = "eth")]
pub trait EthApi {
    #[method(name = "sendUserOperation")]
    async fn send_user_operation(
        &self,
        op: RpcUserOperation,
        entry_point: Address,
    ) -> RpcResult<H256>;

    #[method(name = "estimateUserOperationGas")]
    async fn estimate_user_operation_gas(
        &self,
        op: UserOperationOptionalGas,
        entry_point: Address,
    ) -> RpcResult<GasEstimate>;

    #[method(name = "getUserOperationByHash")]
    async fn get_user_operation_by_hash(&self, hash: H256) -> RpcResult<Option<RichUserOperation>>;

    #[method(name = "getUserOperationReceipt")]
    async fn get_user_operation_receipt(
        &self,
        hash: H256,
    ) -> RpcResult<Option<UserOperationReceipt>>;

    #[method(name = "supportedEntryPoints")]
    async fn supported_entry_points(&self) -> RpcResult<Vec<String>>;

    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<U64>;
}

#[derive(Debug)]
struct EntryPointAndSimulator {
    entry_point: EntryPoint<Provider<Http>>,
    simulator: SimulatorImpl,
}

impl EntryPointAndSimulator {
    pub fn new(
        address: Address,
        provider: Arc<Provider<Http>>,
        sim_settings: simulation::Settings,
    ) -> Self {
        let entry_point = EntryPoint::new(address, Arc::clone(&provider));
        let simulator = SimulatorImpl::new(Arc::clone(&provider), address, sim_settings);
        Self {
            entry_point,
            simulator,
        }
    }
}

pub struct EthApi {
    entry_points_and_sims: HashMap<Address, EntryPointAndSimulator>,
    provider: Arc<Provider<Http>>,
    chain_id: u64,
    op_pool_client: OpPoolClient<Channel>,
}

impl EthApi {
    pub fn new(
        provider: Arc<Provider<Http>>,
        entry_points: Vec<Address>,
        chain_id: u64,
        op_pool_client: OpPoolClient<Channel>,
        sim_settings: simulation::Settings,
    ) -> Self {
        let entry_points_and_sims = entry_points
            .iter()
            .map(|&a| {
                (
                    a,
                    EntryPointAndSimulator::new(a, Arc::clone(&provider), sim_settings),
                )
            })
            .collect();

        Self {
            entry_points_and_sims,
            provider,
            chain_id,
            op_pool_client,
        }
    }

    fn get_entry_point(&self, address: &Address) -> Option<&EntryPoint<Provider<Http>>> {
        self.entry_points_and_sims
            .get(address)
            .map(|eps| &eps.entry_point)
    }

    async fn get_user_operation_event_by_hash(&self, hash: H256) -> anyhow::Result<Option<Log>> {
        let filter = Filter::new()
            .address::<Vec<Address>>(self.entry_points_and_sims.iter().map(|ep| *ep.0).collect())
            .topic1(hash);

        // we don't do .query().await here because we still need the raw logs for the TX
        // hash later. But hopefully this is a bit clearer than using .abi_signature()
        let filter = UserOperationEventFilter::new(filter, &self.provider).filter;

        let logs = self.provider.get_logs(&filter).await?;

        Ok(logs.into_iter().next())
    }

    fn get_user_operations_from_tx_data(
        &self,
        tx_data: Bytes,
    ) -> anyhow::Result<Vec<UserOperation>> {
        let entry_point_calls = EntryPointCalls::decode(tx_data)
            .context("should parse tx data as calls to the entry point")?;

        match entry_point_calls {
            EntryPointCalls::HandleOps(handle_ops_call) => Ok(handle_ops_call.ops),
            EntryPointCalls::HandleAggregatedOps(handle_aggregated_ops_call) => {
                Ok(handle_aggregated_ops_call
                    .ops_per_aggregator
                    .into_iter()
                    .flat_map(|ops| ops.user_ops)
                    .collect())
            }
            _ => bail!("tx should contain a user operations"),
        }
    }

    fn decode_user_operation_event(&self, log: Log) -> anyhow::Result<UserOperationEventFilter> {
        UserOperationEventFilter::decode_log(&log_to_raw_log(log))
            .context("log should be a user operation event")
    }

    /// This method takes a user operation event and a transaction receipt and filters out all the logs
    /// relevant to the user operation. Since there are potentially many user operations in a transaction,
    /// we want to find all the logs (including the user operation event itself) that are sandwiched between
    /// ours and the one before it that wasn't ours.
    /// eg. reference_log: UserOp(hash_moldy) logs: \[...OtherLogs, UserOp(hash1), ...OtherLogs, UserOp(hash_moldy), ...OtherLogs\]
    /// -> logs: logs\[(idx_of_UserOp(hash1) + 1)..=idx_of_UserOp(hash_moldy)\]
    ///
    /// topic\[0\] == event name
    /// topic\[1\] == user operation hash
    ///
    /// NOTE: we can't convert just decode all the logs as user operations and filter because we still want all the other log types
    ///
    fn filter_receipt_logs_matching_user_op(
        reference_log: &Log,
        tx_receipt: &TransactionReceipt,
    ) -> Result<Vec<Log>, anyhow::Error> {
        let mut start_idx = 0;
        let mut end_idx = tx_receipt.logs.len() - 1;
        let logs = &tx_receipt.logs;

        let is_ref_user_op = |log: &Log| {
            log.topics[0] == reference_log.topics[0]
                && log.topics[1] == reference_log.topics[1]
                && log.address == reference_log.address
        };

        let is_user_op_event = |log: &Log| log.topics[0] == reference_log.topics[0];

        let mut i = 0;
        while i < logs.len() {
            if i < end_idx && is_user_op_event(&logs[i]) && !is_ref_user_op(&logs[i]) {
                start_idx = i;
            } else if is_ref_user_op(&logs[i]) {
                end_idx = i;
            }

            i += 1;
        }

        if !is_ref_user_op(&logs[end_idx]) {
            bail!("fatal: no user ops found in tx receipt ({start_idx},{end_idx})")
        }

        let start_idx = if start_idx == 0 { 0 } else { start_idx + 1 };
        Ok(logs[start_idx..=end_idx].to_vec())
    }

    fn get_user_operation_failure_reason(
        logs: &[Log],
        user_op_hash: H256,
    ) -> Result<Option<String>, anyhow::Error> {
        let revert_reason_evt: Option<UserOperationRevertReasonFilter> = logs
            .iter()
            .filter(|l| l.topics[1] == user_op_hash)
            .map_while(|l| {
                UserOperationRevertReasonFilter::decode_log(&RawLog {
                    topics: l.topics.clone(),
                    data: l.data.to_vec(),
                })
                .ok()
            })
            .next();

        revert_reason_evt.map_or(Ok(None), |revert_reason_evt| {
            String::from_utf8(revert_reason_evt.revert_reason.to_vec())
                .map(Some)
                .context("should have parsed revert reason as string")
        })
    }

    async fn get_call_gas_limit(
        &self,
        entry_point: &Address,
        op: &UserOperationOptionalGas,
    ) -> Result<U256, EthRpcError> {
        let entry_point = self
            .get_entry_point(entry_point)
            .context("entry point should exist in the map")?;

        let tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
            to: Some(op.sender.into()),
            from: Some(entry_point.address()),
            data: Some(op.cheap_clone().call_data),
            ..Default::default()
        });

        let call_gas_limit =
            self.provider
                .estimate_gas(&tx, None)
                .await
                .map_err(|pe| match pe {
                    ProviderError::JsonRpcClientError(e) => {
                        EthRpcError::ExecutionReverted(e.to_string())
                    }
                    _ => EthRpcError::Internal(pe.into()),
                })?;

        Ok(call_gas_limit)
    }

    async fn get_verification_gas_limit(
        &self,
        entry_point: &Address,
        op: &UserOperationOptionalGas,
    ) -> Result<U256, EthRpcError> {
        let entry_point = self
            .get_entry_point(entry_point)
            .context("entry point should exist in the map")?;

        let sim_result = entry_point
            .simulate_validation(op.cheap_clone().into())
            .call()
            .await
            .err()
            .context("simulation result should have reverted in entry point")?;

        let provider_error = match sim_result {
            ContractError::MiddlewareError(e) => e,
            ContractError::ProviderError(e) => e,
            _ => {
                tracing::debug!("unexpected sim_result error: {sim_result:?}");
                Err(EthRpcError::Internal(sim_result.into()))?
            }
        };

        let revert_data: EntryPointErrors = get_revert_data(provider_error)
            .map(EntryPointErrors::decode_hex)
            .log_context("should have received provider error containing revert data")?
            .context("returned revert data should match entry point error")?;

        let validation_result = match revert_data {
            EntryPointErrors::ValidationResult(validation_result) => validation_result,
            EntryPointErrors::FailedOp(op) => {
                Err(EthRpcError::EntrypointValidationRejected(op.reason))?
            }
            _ => Err(anyhow!(revert_data))?,
        };

        Ok(validation_result.return_info.0)
    }
}

const EXPIRATION_BUFFER: Duration = Duration::from_secs(30);
#[async_trait]
impl EthApiServer for EthApi {
    async fn send_user_operation(
        &self,
        op: RpcUserOperation,
        entry_point: Address,
    ) -> RpcResult<H256> {
        debug!("send_user_operation: {:?}", op);

        if !self.entry_points_and_sims.contains_key(&entry_point) {
            return Err(EthRpcError::InvalidParams(
                "supplied entry_point addr is not a known entry point".to_string(),
            ))?;
        }

        let op: UserOperation = op.into();

        let EntryPointAndSimulator {
            simulator,
            entry_point,
        } = self
            .entry_points_and_sims
            .get(&entry_point)
            .context("entry point should exist in the map")?;

        // 1. validate op
        let SimulationSuccess {
            valid_time_range,
            code_hash,
            aggregator_address,
            expected_storage_slots,
            signature_failed,
            entities_needing_stake,
            block_hash,
            account_is_staked,
            ..
        } = simulator
            .simulate_validation(op.clone(), None, None)
            .await
            .map_err(EthRpcError::from)?;

        if signature_failed {
            Err(EthRpcError::SignatureCheckFailed)?
        }

        let now = Timestamp::now();
        let valid_after = valid_time_range.valid_after;
        let valid_until = valid_time_range.valid_until;
        if !valid_time_range.contains(now, EXPIRATION_BUFFER) {
            Err(EthRpcError::OutOfTimeRange(OutOfTimeRangeData {
                valid_after,
                valid_until,
                paymaster: op.paymaster(),
            }))?
        }

        // 2. send op the mempool
        let add_op_result = self
            .op_pool_client
            .clone()
            .add_op(AddOpRequest {
                entry_point: entry_point.address().as_bytes().to_vec(),
                op: Some(MempoolOp {
                    uo: Some((&op).into()),
                    aggregator: aggregator_address.unwrap_or_default().as_bytes().to_vec(),
                    valid_after: valid_after.seconds_since_epoch(),
                    valid_until: valid_until.seconds_since_epoch(),
                    expected_code_hash: code_hash.as_bytes().to_vec(),
                    sim_block_hash: block_hash.as_bytes().to_vec(),
                    entities_needing_stake: entities_needing_stake
                        .iter()
                        .map(|&e| OpPoolEntity::from(e).into())
                        .collect(),
                    expected_storage_slots: expected_storage_slots
                        .iter()
                        .map(|ss| StorageSlot {
                            address: ss.address.as_bytes().to_vec(),
                            slot: to_le_bytes(ss.slot),
                            value: to_le_bytes(ss.value),
                        })
                        .collect(),
                    account_is_staked,
                }),
            })
            .await
            .map_err(EthRpcError::from)
            .log_on_error_level(Level::DEBUG, "failed to add op to the mempool")?;

        // 3. return the op hash
        Ok(H256::from_slice(&add_op_result.into_inner().hash))
    }

    async fn estimate_user_operation_gas(
        &self,
        op: UserOperationOptionalGas,
        entry_point: Address,
    ) -> RpcResult<GasEstimate> {
        if !self.entry_points_and_sims.contains_key(&entry_point) {
            return Err(EthRpcError::InvalidParams(
                "supplied entry_point addr is not a known entry point".to_string(),
            ))?;
        }

        let pre_verification_gas = op.calc_pre_verification_gas();
        let call_gas_limit = self.get_call_gas_limit(&entry_point, &op);
        let verification_gas = self.get_verification_gas_limit(&entry_point, &op);

        let (call_gas_limit, verification_gas) = join!(call_gas_limit, verification_gas);

        let verification_gas =
            verification_gas.log_on_error("should have computed verification gas successfully")?;
        let call_gas_limit =
            call_gas_limit.log_on_error("should have computed call gas limit successfully")?;

        Ok(GasEstimate {
            call_gas_limit,
            verification_gas,
            pre_verification_gas,
        })
    }

    async fn get_user_operation_by_hash(&self, hash: H256) -> RpcResult<Option<RichUserOperation>> {
        if hash == H256::zero() {
            return Err(EthRpcError::InvalidParams(
                "hash cannot be zero".to_string(),
            ))?;
        }

        // 1. Get event associated with hash (need to check all entry point addresses associated with this API)
        let event = self
            .get_user_operation_event_by_hash(hash)
            .await
            .context("should have successfully queried for user op events by hash")?;

        let Some(event) = event else {
            return Ok(None)
        };

        // 2. If the event is found, get the TX
        let transaction_hash = event
            .transaction_hash
            .context("tx_hash should be present")?;

        let tx = self
            .provider
            .get_transaction(transaction_hash)
            .await
            .context("should have fetched tx from provider")?
            .context("should have found tx")?;

        // We should return null if the tx isn't included in the block yet
        if tx.block_hash.is_none() && tx.block_number.is_none() {
            return Ok(None);
        }

        let to = tx
            .to
            .filter(|to| self.entry_points_and_sims.contains_key(to))
            .context("Failed to parse tx or tx doesn't belong to entry point")?;

        // 3. parse the tx data using the EntryPoint interface and extract UserOperation[] from it
        let user_ops = self
            .get_user_operations_from_tx_data(tx.input)
            .context("should have parsed tx data as user operations")?;

        // 4. find first op matching sender + nonce with the event
        let event = self
            .decode_user_operation_event(event)
            .context("should have decoded log event as user operation event")?;

        let user_operation = user_ops
            .into_iter()
            .find(|op| op.sender == event.sender && op.nonce == event.nonce)
            .context("matching user operation should be found in tx data")?;

        // 5. return the result
        Ok(Some(RichUserOperation {
            user_operation: user_operation.into(),
            entry_point: to.into(),
            block_number: tx
                .block_number
                .map(|n| U256::from(n.as_u64()))
                .unwrap_or_default(),
            block_hash: tx.block_hash.unwrap_or_default(),
            transaction_hash,
        }))
    }

    async fn get_user_operation_receipt(
        &self,
        hash: H256,
    ) -> RpcResult<Option<UserOperationReceipt>> {
        if hash == H256::zero() {
            return Err(EthRpcError::InvalidParams(
                "Missing/invalid userOpHash".to_string(),
            ))?;
        }

        // 1. Get event associated with hash (need to check all entry point addresses associated with this API)
        let log = self
            .get_user_operation_event_by_hash(hash)
            .await
            .context("should have fetched user ops by hash")?;

        let Some(log) = log else {
            return Ok(None)
        };

        // 2. If the event is found, get the TX receipt
        let tx_hash = log.transaction_hash.context("tx_hash should be present")?;

        let tx_receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await
            .context("should have fetched tx receipt")?
            .context("Failed to fetch tx receipt")?;

        // We should return null if the tx isn't included in the block yet
        if tx_receipt.block_hash.is_none() && tx_receipt.block_number.is_none() {
            return Ok(None);
        }

        let to = tx_receipt
            .to
            .filter(|to| self.entry_points_and_sims.contains_key(to))
            .context("Failed to parse tx or tx doesn't belong to entry point")?;

        // 3. filter receipt logs to match just those belonging to the user op
        let filtered_logs = EthApi::filter_receipt_logs_matching_user_op(&log, &tx_receipt)
            .context("should have found receipt logs matching user op")?;

        // 4. decode log and find failure reason if not success
        let log = self
            .decode_user_operation_event(log)
            .context("should have decoded user operation event")?;

        let reason: Option<String> = if log.success {
            None
        } else {
            EthApi::get_user_operation_failure_reason(&tx_receipt.logs, hash)
                .context("should have found revert reason if tx wasn't successful")?
        };

        // 5. Return the result
        Ok(Some(UserOperationReceipt {
            user_op_hash: hash,
            entry_point: to.into(),
            sender: log.sender.into(),
            nonce: log.nonce,
            paymaster: log.paymaster.into(),
            actual_gas_cost: log.actual_gas_cost,
            acutal_gas_used: log.actual_gas_used,
            success: log.success,
            logs: filtered_logs,
            receipt: tx_receipt,
            reason,
        }))
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<String>> {
        Ok(self
            .entry_points_and_sims
            .keys()
            .map(|ep| to_checksum(ep, None))
            .collect())
    }

    async fn chain_id(&self) -> RpcResult<U64> {
        Ok(self.chain_id.into())
    }
}

impl From<SimulationError> for EthRpcError {
    fn from(mut value: SimulationError) -> Self {
        debug!("Simulation error: {value:?}");

        // TODO: handle unsupported signature aggregator (assuming not throttled/banned and staked)
        // this requires calling aggregator.validateUserOpSignature
        let SimulationError::Violations(violations) = &mut value else {
            return Self::Internal(value.into())
        };

        let Some(violation) = violations.iter().min() else {
            return Self::Internal(value.into())
        };

        match violation {
            Violation::UnintendedRevertWithMessage(Entity::Paymaster, reason, Some(paymaster)) => {
                Self::PaymasterValidationRejected(PaymasterValidationRejectedData {
                    paymaster: *paymaster,
                    reason: reason.to_string(),
                })
            }
            Violation::UnintendedRevertWithMessage(_, reason, _) => {
                Self::EntrypointValidationRejected(reason.clone())
            }
            Violation::UsedForbiddenOpcode(entity, op) => {
                Self::OpcodeViolation(*entity, op.clone().0)
            }
            Violation::InvalidGasOpcode(entity) => Self::OpcodeViolation(*entity, OpCode::GAS),
            Violation::FactoryCalledCreate2Twice => {
                Self::OpcodeViolation(Entity::Factory, OpCode::CREATE2)
            }
            Violation::InvalidStorageAccess(entity, address) => {
                Self::InvalidStorageAccess(*entity, *address)
            }
            Violation::NotStaked(entity, address, min_stake, min_unstake_delay) => {
                let err_data = match entity {
                    Entity::Account => {
                        StakeTooLowData::account(*address, *min_stake, *min_unstake_delay)
                    }
                    Entity::Paymaster => {
                        StakeTooLowData::paymaster(*address, *min_stake, *min_unstake_delay)
                    }
                    Entity::Aggregator => {
                        StakeTooLowData::aggregator(*address, *min_stake, *min_unstake_delay)
                    }
                    Entity::Factory => {
                        StakeTooLowData::factory(*address, *min_stake, *min_unstake_delay)
                    }
                };

                Self::StakeTooLow(err_data)
            }
            _ => Self::Internal(value.into()),
        }
    }
}

impl TryFrom<Status> for ErrorInfo {
    type Error = anyhow::Error;

    fn try_from(value: Status) -> Result<Self, Self::Error> {
        let decoded_status =
            tonic_types::Status::decode(value.details()).context("should have decoded status")?;

        // This could actually contain more error details, but we only use the first right now
        let any = decoded_status
            .details
            .first()
            .context("should have details")?;

        if any.type_url == "type.alchemy.com/op_pool.ErrorInfo" {
            ErrorInfo::decode(&any.value[..])
                .context("should have decoded successfully into ErrorInfo")
        } else {
            bail!("Unknown type_url: {}", any.type_url);
        }
    }
}

impl From<ErrorInfo> for EthRpcError {
    fn from(value: ErrorInfo) -> Self {
        let ErrorInfo { reason, metadata } = value;

        if reason == ErrorReason::EntityThrottled.as_str_name() {
            let (entity, address) = metadata.iter().next().unwrap();
            let Some(address) = Address::from_str(address).ok() else {
                return anyhow!("should have valid address in ErrorInfo metadata").into()
            };

            let Some(entity) = Entity::from_str(entity).ok() else {
                return anyhow!("should be a valid Entity type in ErrorInfo metadata").into()
            };

            let data = match entity {
                Entity::Aggregator => ThrottledOrBannedData::aggregator(address),
                Entity::Paymaster => ThrottledOrBannedData::paymaster(address),
                Entity::Factory => ThrottledOrBannedData::factory(address),
                _ => return anyhow!("unsupported entity {} as throttled reason", entity).into(),
            };

            return EthRpcError::ThrottledOrBanned(data);
        } else if reason == ErrorReason::ReplacementUnderpriced.as_str_name() {
            return EthRpcError::ReplacementUnderpriced;
        }

        anyhow!("operation rejected").into()
    }
}

impl From<Status> for EthRpcError {
    fn from(status: Status) -> Self {
        let status_details: anyhow::Result<ErrorInfo> = status.try_into();
        let Ok(error_info) = status_details else {
            return EthRpcError::Internal(status_details.unwrap_err())
        };

        EthRpcError::from(error_info)
    }
}

#[cfg(test)]
mod tests {
    use crate::{common::protos::op_pool::ErrorReason, rpc::eth::error::ThrottledOrBannedData};

    use super::*;
    use ethers::{
        types::{Log, TransactionReceipt},
        utils::{hex::ToHex, keccak256},
    };

    const UO_OP_TOPIC: &str = "user-op-event-topic";

    #[test]
    fn test_throttled_or_banned_decode() {
        let error_info = ErrorInfo {
            reason: ErrorReason::EntityThrottled.as_str_name().to_string(),
            metadata: HashMap::from([(
                Entity::Paymaster.to_string(),
                Address::default().encode_hex(),
            )]),
        };

        let details = tonic_types::Status {
            code: 0,
            message: "".to_string(),
            details: vec![prost_types::Any {
                type_url: "type.alchemy.com/op_pool.ErrorInfo".to_string(),
                value: error_info.encode_to_vec(),
            }],
        };

        let status = Status::with_details(
            tonic::Code::Internal,
            "error_message".to_string(),
            details.encode_to_vec().into(),
        );

        let rpc_error: EthRpcError = status.into();

        assert!(
            matches!(
                rpc_error,
                EthRpcError::ThrottledOrBanned(data) if data == ThrottledOrBannedData::paymaster(Address::default())
            ),
            "{:?}",
            rpc_error
        );
    }

    #[test]
    fn test_filter_receipt_logs_when_at_begining_of_list() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");
        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            reference_log.clone(),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log(UO_OP_TOPIC, "another-hash"),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_ok(), "{}", result.unwrap_err());
        let result = result.unwrap();
        assert_eq!(result, receipt.logs[0..=1]);
    }

    #[test]
    fn test_filter_receipt_logs_when_in_middle_of_list() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");
        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log("another-topic", "some-hash"),
            given_log("another-topic-2", "some-hash"),
            reference_log.clone(),
            given_log(UO_OP_TOPIC, "another-hash"),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_ok(), "{}", result.unwrap_err());
        let result = result.unwrap();
        assert_eq!(result, receipt.logs[2..=4]);
    }

    #[test]
    fn test_filter_receipt_logs_when_at_end_of_list() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");
        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log(UO_OP_TOPIC, "another-hash"),
            given_log("another-topic", "some-hash"),
            given_log("another-topic-2", "some-hash"),
            reference_log.clone(),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_ok(), "{}", result.unwrap_err());
        let result = result.unwrap();
        assert_eq!(result, receipt.logs[3..=5]);
    }

    #[test]
    fn test_filter_receipt_logs_skips_event_from_different_address() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");
        let mut reference_log_w_different_address = reference_log.clone();
        reference_log_w_different_address.address = Address::from_low_u64_be(0x1234);

        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log(UO_OP_TOPIC, "another-hash"),
            reference_log_w_different_address,
            given_log("another-topic", "some-hash"),
            given_log("another-topic-2", "some-hash"),
            reference_log.clone(),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_ok(), "{}", result.unwrap_err());
        let result = result.unwrap();
        assert_eq!(result, receipt.logs[4..=6]);
    }

    #[test]
    fn test_filter_receipt_logs_includes_multiple_sets_of_ref_uo() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");

        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log("other-topic-2", "another-hash"),
            reference_log.clone(),
            given_log("another-topic", "some-hash"),
            given_log("another-topic-2", "some-hash"),
            reference_log.clone(),
            given_log(UO_OP_TOPIC, "other-hash"),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_ok(), "{}", result.unwrap_err());
        let result = result.unwrap();
        assert_eq!(result, receipt.logs[2..=6]);
    }

    #[test]
    fn test_filter_receipt_logs_when_not_found() {
        let reference_log = given_log(UO_OP_TOPIC, "moldy-hash");
        let receipt = given_receipt(vec![
            given_log("other-topic", "some-hash"),
            given_log(UO_OP_TOPIC, "other-hash"),
            given_log(UO_OP_TOPIC, "another-hash"),
            given_log("another-topic", "some-hash"),
            given_log("another-topic-2", "some-hash"),
        ]);

        let result = EthApi::filter_receipt_logs_matching_user_op(&reference_log, &receipt);

        assert!(result.is_err(), "{:?}", result.unwrap());
    }

    fn given_log(topic_0: &str, topic_1: &str) -> Log {
        Log {
            topics: vec![
                keccak256(topic_0.as_bytes()).into(),
                keccak256(topic_1.as_bytes()).into(),
            ],
            ..Default::default()
        }
    }

    fn given_receipt(logs: Vec<Log>) -> TransactionReceipt {
        TransactionReceipt {
            logs,
            ..Default::default()
        }
    }
}
