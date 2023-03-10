use clarity::Address as EthAddress;
use clarity::{PrivateKey as EthPrivateKey, Signature};
use deep_space::address::Address as CosmosAddress;
use deep_space::error::CosmosGrpcError;
use deep_space::private_key::PrivateKey;
use deep_space::Contact;
use deep_space::Msg;
use deep_space::{coin::Coin, utils::bytes_to_hex_str};
use ethereum_gravity::message_signatures::{
    encode_logic_call_confirm, encode_tx_batch_confirm, encode_valset_confirm,
};
use gravity_proto::cosmos_sdk_proto::cosmos::base::abci::v1beta1::TxResponse;
use gravity_proto::gravity::Erc20Token as ProtoErc20Token;
use gravity_proto::gravity::{
    MsgCancelSendToEth, MsgConfirmBatch, MsgConfirmLogicCall, MsgExecuteIbcAutoForwards,
    MsgRequestBatch, MsgSendToEth, MsgSetOrchestratorAddress, MsgSubmitBadSignatureEvidence,
    MsgValsetConfirm,
};

use gravity_utils::error::GravityError;
use gravity_utils::types::{
    Erc20DeployedEvent, EthereumEvent, LogicCall, LogicCallExecutedEvent, SendToCosmosEvent,
    TransactionBatch, TransactionBatchExecutedEvent, Valset, ValsetUpdatedEvent,
};
use num256::Uint256;
use std::{collections::HashMap, time::Duration};
use web30::client::Web3;
use web30::jsonrpc::error::Web3Error;

use crate::utils::get_reasonable_send_to_eth_fee;
use crate::utils::{
    collect_eth_balances_for_claims, get_gravity_monitored_erc20s, BadSignatureEvidence,
};

pub const MEMO: &str = "Sent using Althea Gravity Bridge Orchestrator";
pub const TIMEOUT: Duration = Duration::from_secs(60);

// gravity msg type urls
pub const MSG_SET_ORCHESTRATOR_ADDRESS_TYPE_URL: &str = "/gravity.v1.MsgSetOrchestratorAddress";
pub const MSG_VALSET_CONFIRM_TYPE_URL: &str = "/gravity.v1.MsgValsetConfirm";
pub const MSG_CONFIRM_BATCH_TYPE_URL: &str = "/gravity.v1.MsgConfirmBatch";
pub const MSG_CONFIRM_LOGIC_CALL_TYPE_URL: &str = "/gravity.v1.MsgConfirmLogicCall";
pub const MSG_SEND_TO_ETH_TYPE_URL: &str = "/gravity.v1.MsgSendToEth";
pub const MSG_REQUEST_BATCH_TYPE_URL: &str = "/gravity.v1.MsgRequestBatch";
pub const MSG_SUBMIT_BAD_SIGNATURE_EVIDENCE_TYPE_URL: &str =
    "/gravity.v1.MsgSubmitBadSignatureEvidence";
pub const MSG_CANCEL_SEND_TO_ETH_TYPE_URL: &str = "/gravity.v1.MsgCancelSendToEth";
pub const MSG_EXECUTE_IBC_AUTO_FORWARDS_TYPE_URL: &str = "/gravity.v1.MsgExecuteIbcAutoForwards";

/// Send a transaction updating the eth address for the sending
/// Cosmos address. The sending Cosmos address should be a validator
/// this can only be called once! Key rotation code is possible but
/// not currently implemented
pub async fn set_gravity_delegate_addresses(
    contact: &Contact,
    delegate_eth_address: EthAddress,
    delegate_cosmos_address: CosmosAddress,
    private_key: impl PrivateKey,
    fee: Coin,
) -> Result<TxResponse, CosmosGrpcError> {
    trace!("Updating Gravity Delegate addresses");
    let our_valoper_address = private_key
        .to_address(&contact.get_prefix())
        .unwrap()
        // This works so long as the format set by the cosmos hub is maintained
        // having a main prefix followed by a series of titles for specific keys
        // this will not work if that convention is broken. This will be resolved when
        // GRPC exposes prefix endpoints (coming to upstream cosmos sdk soon)
        .to_bech32(format!("{}valoper", contact.get_prefix()))
        .unwrap();

    let msg_set_orch_address = MsgSetOrchestratorAddress {
        validator: our_valoper_address.to_string(),
        orchestrator: delegate_cosmos_address.to_string(),
        eth_address: delegate_eth_address.to_string(),
    };

    let msg = Msg::new(MSG_SET_ORCHESTRATOR_ADDRESS_TYPE_URL, msg_set_orch_address);
    contact
        .send_message(
            &[msg],
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Send in a confirmation for an array of validator sets, it's far more efficient to send these
/// as a single message
#[allow(clippy::too_many_arguments)]
pub async fn send_valset_confirms(
    contact: &Contact,
    eth_private_key: EthPrivateKey,
    fee: Coin,
    valsets: Vec<Valset>,
    private_key: impl PrivateKey,
    gravity_id: String,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();
    let our_eth_address = eth_private_key.to_address();

    let mut messages = Vec::new();

    for valset in valsets {
        trace!("Submitting signature for valset {:?}", valset);
        let message = encode_valset_confirm(gravity_id.clone(), valset.clone());
        let eth_signature = eth_private_key.sign_ethereum_msg(&message);
        trace!(
            "Sending valset update with address {} and sig {}",
            our_eth_address,
            bytes_to_hex_str(&eth_signature.to_bytes())
        );
        let confirm = MsgValsetConfirm {
            orchestrator: our_address.to_string(),
            eth_address: our_eth_address.to_string(),
            nonce: valset.nonce,
            signature: bytes_to_hex_str(&eth_signature.to_bytes()),
        };
        let msg = Msg::new(MSG_VALSET_CONFIRM_TYPE_URL, confirm);
        messages.push(msg);
    }
    let res = contact
        .send_message(
            &messages,
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await;
    info!("Valset confirm res is {:?}", res);
    res
}

/// Send in a confirmation for a specific transaction batch
pub async fn send_batch_confirm(
    contact: &Contact,
    eth_private_key: EthPrivateKey,
    fee: Coin,
    transaction_batches: Vec<TransactionBatch>,
    private_key: impl PrivateKey,
    gravity_id: String,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();
    let our_eth_address = eth_private_key.to_address();

    let mut messages = Vec::new();

    for batch in transaction_batches {
        trace!("Submitting signature for batch {:?}", batch);
        let message = encode_tx_batch_confirm(gravity_id.clone(), batch.clone());
        let eth_signature = eth_private_key.sign_ethereum_msg(&message);
        trace!(
            "Sending batch update with address {} and sig {}",
            our_eth_address,
            bytes_to_hex_str(&eth_signature.to_bytes())
        );
        let confirm = MsgConfirmBatch {
            token_contract: batch.token_contract.to_string(),
            orchestrator: our_address.to_string(),
            eth_signer: our_eth_address.to_string(),
            nonce: batch.nonce,
            signature: bytes_to_hex_str(&eth_signature.to_bytes()),
        };
        let msg = Msg::new(MSG_CONFIRM_BATCH_TYPE_URL, confirm);
        messages.push(msg);
    }
    contact
        .send_message(
            &messages,
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Send in a confirmation for a specific logic call
pub async fn send_logic_call_confirm(
    contact: &Contact,
    eth_private_key: EthPrivateKey,
    fee: Coin,
    logic_calls: Vec<LogicCall>,
    private_key: impl PrivateKey,
    gravity_id: String,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();
    let our_eth_address = eth_private_key.to_address();

    let mut messages = Vec::new();

    for call in logic_calls {
        trace!("Submitting signature for LogicCall {:?}", call);
        let message = encode_logic_call_confirm(gravity_id.clone(), call.clone());
        let eth_signature = eth_private_key.sign_ethereum_msg(&message);
        trace!(
            "Sending LogicCall update with address {} and sig {}",
            our_eth_address,
            bytes_to_hex_str(&eth_signature.to_bytes())
        );
        let confirm = MsgConfirmLogicCall {
            orchestrator: our_address.to_string(),
            eth_signer: our_eth_address.to_string(),
            signature: bytes_to_hex_str(&eth_signature.to_bytes()),
            invalidation_id: bytes_to_hex_str(&call.invalidation_id),
            invalidation_nonce: call.invalidation_nonce,
        };
        let msg = Msg::new(MSG_CONFIRM_LOGIC_CALL_TYPE_URL, confirm);
        messages.push(msg);
    }
    contact
        .send_message(
            &messages,
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Creates and submits Ethereum event claims from the input EthereumEvent collections
/// If Orchestrators are required to submit Gravity.sol balances, it is possible that not all claims
/// will be submitted due to historical ERC20 balance query failures.
/// If no claims are submitted due to lack of ERC20 balances, Ok(None) will be returned
#[allow(clippy::too_many_arguments)]
pub async fn send_ethereum_claims(
    web3: &Web3,
    contact: &Contact,
    gravity_contract: EthAddress,
    cosmos_private_key: impl PrivateKey,
    our_eth_address: EthAddress,
    deposits: Vec<SendToCosmosEvent>,
    withdraws: Vec<TransactionBatchExecutedEvent>,
    erc20_deploys: Vec<Erc20DeployedEvent>,
    logic_calls: Vec<LogicCallExecutedEvent>,
    valsets: Vec<ValsetUpdatedEvent>,
    fee: Coin,
) -> Result<Option<TxResponse>, GravityError> {
    let our_cosmos_address = cosmos_private_key
        .to_address(&contact.get_prefix())
        .unwrap();

    let monitored_erc20s = get_gravity_monitored_erc20s(contact.get_url()).await?;
    let must_monitor_erc20s = !monitored_erc20s.is_empty();

    // If the gov param is populated, Orchestrators are required to submit the Gravity.sol balances for various ERC20s
    let eth_balances_by_block_height = if must_monitor_erc20s {
        Some(
            collect_eth_balances_for_claims(
                web3,
                our_eth_address,
                gravity_contract,
                monitored_erc20s,
                &deposits,
                &withdraws,
                &erc20_deploys,
                &logic_calls,
                &valsets,
            )
            .await?,
        )
    } else {
        None
    };

    // Error if ERC20s should be monitored but we have no balances to report
    if must_monitor_erc20s
        && (eth_balances_by_block_height.is_none()
            || eth_balances_by_block_height.clone().unwrap().is_empty())
    {
        return Err(GravityError::EthereumRestError(Web3Error::BadResponse(
            "Could not obtain historical Gravity.sol Eth balances to report in claims".to_string(),
        )));
    }

    // This sorts oracle messages by event nonce before submitting them. It's not a pretty implementation because
    // we're missing an intermediary layer of abstraction. We could implement 'EventTrait' and then implement sort
    // for it, but then when we go to transform 'EventTrait' objects into GravityMsg enum values we'll have all sorts
    // of issues extracting the inner object from the TraitObject. Likewise we could implement sort of GravityMsg but that
    // would require a truly horrendous (nearly 100 line) match statement to deal with all combinations. That match statement
    // could be reduced by adding two traits to sort against but really this is the easiest option.
    //
    // We index the events by event nonce in an unordered hashmap and then play them back in order into a vec
    let mut unordered_msgs = HashMap::new();

    // Create claim Msgs, keeping their event_nonces for insertion into unordered_msgs
    let deposit_nonces_msgs: Vec<(u64, Msg)> = create_claim_msgs(
        eth_balances_by_block_height.clone(),
        deposits,
        our_cosmos_address,
    );
    let withdraw_nonces_msgs: Vec<(u64, Msg)> = create_claim_msgs(
        eth_balances_by_block_height.clone(),
        withdraws,
        our_cosmos_address,
    );
    let deploy_nonces_msgs: Vec<(u64, Msg)> = create_claim_msgs(
        eth_balances_by_block_height.clone(),
        erc20_deploys,
        our_cosmos_address,
    );
    let logic_nonces_msgs: Vec<(u64, Msg)> = create_claim_msgs(
        eth_balances_by_block_height.clone(),
        logic_calls,
        our_cosmos_address,
    );
    let valset_nonces_msgs: Vec<(u64, Msg)> = create_claim_msgs(
        eth_balances_by_block_height.clone(),
        valsets,
        our_cosmos_address,
    );

    // Collect all of the (nonces, claims) into an iterator, then add them to unordered_msgs
    deposit_nonces_msgs
        .into_iter()
        .chain(withdraw_nonces_msgs.into_iter())
        .chain(deploy_nonces_msgs.into_iter())
        .chain(logic_nonces_msgs.into_iter())
        .chain(valset_nonces_msgs.into_iter())
        .map(|(nonce, msg)| assert!(unordered_msgs.insert(nonce, msg).is_none()))
        .for_each(drop); // Exhaust the iterator so that `unordered_msgs` is populated from .map()

    if unordered_msgs.is_empty() {
        // No messages to send, return early
        info!(concat!(
            "Unable to send ethereum claims because monitored Gravity.sol balances could not be ",
            "collected. If this message appears repeatedly, check your Eth connection."
        ));
        return Ok(None);
    }

    let mut keys = Vec::new();
    for (key, _) in unordered_msgs.iter() {
        keys.push(*key);
    }
    // sorts ascending by default
    keys.sort_unstable();

    const MAX_ORACLE_MESSAGES: usize = 1000;
    let mut msgs = Vec::new();
    for i in keys {
        // pushes messages with a later nonce onto the end
        msgs.push(unordered_msgs.remove_entry(&i).unwrap().1);
    }
    // prevents the message buffer from getting too big if a lot of events
    // are left in a validators queue
    while msgs.len() > MAX_ORACLE_MESSAGES {
        // pops messages off of the end
        msgs.pop();
    }

    contact
        .send_message(&msgs, None, &[fee], Some(TIMEOUT), cosmos_private_key)
        .await
        .map(Some)
        .map_err(GravityError::CosmosGrpcError)
}

/// Creates the `Msg`s needed for `orchestrator` to attest to `events`
/// If `eth_balances_by_block_height` is None, the `Msg`s will have empty `bridge_balances`,
/// otherwise if no balances exist for any event's block_height, that event will be skipped
/// Returns a Vec of (event_nonce: u64, Msg), which may be empty
fn create_claim_msgs(
    eth_balances_by_block_height: Option<HashMap<Uint256, Vec<ProtoErc20Token>>>,
    events: Vec<impl EthereumEvent>,
    orchestrator: CosmosAddress,
) -> Vec<(u64, Msg)> {
    let mut msgs = vec![];
    for event in events {
        // Get the required eth balances for this height if they exist
        let eth_balances: Option<Vec<ProtoErc20Token>> = if eth_balances_by_block_height.is_none() {
            Some(vec![]) // Not required to submit balances
        } else {
            let height: Uint256 = event.get_block_height().into();
            // Required to submit balances, None -> skip and try again later
            eth_balances_by_block_height
                .as_ref()
                .unwrap()
                .get(&height)
                .cloned()
        };

        match eth_balances {
            Some(eth_bals) => {
                info!("Adding bridge_balances to Msg: {:?}", eth_bals);
                // Create msg
                msgs.push((
                    event.get_event_nonce(),
                    event.to_claim_msg(orchestrator, eth_bals.clone()),
                ));
            }
            None => continue,
        }
    }
    msgs
}

/// Sends tokens from Cosmos to Ethereum. These tokens will not be sent immediately instead
/// they will require some time to be included in a batch. Note that there are three fees:
/// bridge_fee: the fee to be sent to Ethereum, which must be the same denom as the amount
/// chain_fee: the Gravity chain fee, which must be the same denom as the amount and which
///     must also meet the governance-defined minimum percentage of the amount
/// cosmos_fee: the Cosmos anti-spam fee set by each Validator which is required for any Tx
///     to be considered for the mempool.
pub async fn send_to_eth(
    private_key: impl PrivateKey,
    destination: EthAddress,
    amount: Coin,
    bridge_fee: Coin,
    chain_fee: Option<Coin>,
    fee: Coin,
    contact: &Contact,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();
    if amount.denom != bridge_fee.denom {
        return Err(CosmosGrpcError::BadInput(format!(
            "{} {} is an invalid denom set for SendToEth you must pay ethereum fees in the same token your sending",
            amount.denom, bridge_fee.denom,
        )));
    }
    let chain_fee = match chain_fee {
        Some(fee) => fee,
        None => Coin {
            amount: get_reasonable_send_to_eth_fee(contact, amount.amount)
                .await
                .expect("Unable to get reasonable SendToEth fee"),
            denom: amount.denom.clone(),
        },
    };
    if amount.denom != chain_fee.denom {
        return Err(CosmosGrpcError::BadInput(format!(
            "{} {} is an invalid denom set for SendToEth you must pay chain fees in the same token your sending",
            amount.denom, chain_fee.denom,
        )));
    }
    let balances = contact.get_balances(our_address).await.unwrap();
    let mut found = false;
    for balance in balances {
        if balance.denom == amount.denom {
            let total_amount = amount.amount + (fee.amount * 2u8.into());
            if balance.amount < total_amount {
                return Err(CosmosGrpcError::BadInput(format!(
                    "Insufficient balance of {} to send {}",
                    amount.denom, total_amount,
                )));
            }
            found = true;
        }
    }
    if !found {
        return Err(CosmosGrpcError::BadInput(format!(
            "No balance of {} to send",
            amount.denom,
        )));
    }

    let msg_send_to_eth = MsgSendToEth {
        sender: our_address.to_string(),
        eth_dest: destination.to_string(),
        amount: Some(amount.into()),
        bridge_fee: Some(bridge_fee.into()),
        chain_fee: Some(chain_fee.into()),
    };
    info!(
        "Sending to Ethereum with MsgSendToEth: {:?}",
        msg_send_to_eth
    );

    let msg = Msg::new(MSG_SEND_TO_ETH_TYPE_URL, msg_send_to_eth);
    contact
        .send_message(
            &[msg],
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

pub async fn send_request_batch(
    private_key: impl PrivateKey,
    denom: String,
    fee: Option<Coin>,
    contact: &Contact,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();

    let msg_request_batch = MsgRequestBatch {
        sender: our_address.to_string(),
        denom,
    };
    let msg = Msg::new(MSG_REQUEST_BATCH_TYPE_URL, msg_request_batch);

    let fee: Vec<Coin> = match fee {
        Some(fee) => vec![fee],
        None => vec![],
    };
    contact
        .send_message(
            &[msg],
            Some(MEMO.to_string()),
            &fee,
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Sends evidence of a bad signature to the chain to slash the malicious validator
/// who signed an invalid message with their Ethereum key
pub async fn submit_bad_signature_evidence(
    private_key: impl PrivateKey,
    fee: Coin,
    contact: &Contact,
    signed_object: BadSignatureEvidence,
    signature: Signature,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();

    let any = signed_object.to_any();

    let msg_submit_bad_signature_evidence = MsgSubmitBadSignatureEvidence {
        subject: Some(any),
        signature: bytes_to_hex_str(&signature.to_bytes()),
        sender: our_address.to_string(),
    };

    let msg = Msg::new(
        MSG_SUBMIT_BAD_SIGNATURE_EVIDENCE_TYPE_URL,
        msg_submit_bad_signature_evidence,
    );
    contact
        .send_message(
            &[msg],
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Cancels a user provided SendToEth transaction, provided it's not already in a batch
/// you should check with `QueryPendingSendToEth`
pub async fn cancel_send_to_eth(
    private_key: impl PrivateKey,
    fee: Coin,
    contact: &Contact,
    transaction_id: u64,
) -> Result<TxResponse, CosmosGrpcError> {
    let our_address = private_key.to_address(&contact.get_prefix()).unwrap();

    let msg_cancel_send_to_eth = MsgCancelSendToEth {
        transaction_id,
        sender: our_address.to_string(),
    };

    let msg = Msg::new(MSG_CANCEL_SEND_TO_ETH_TYPE_URL, msg_cancel_send_to_eth);
    contact
        .send_message(
            &[msg],
            Some(MEMO.to_string()),
            &[fee],
            Some(TIMEOUT),
            private_key,
        )
        .await
}

/// Executes a MsgExecuteIbcAutoForwards on the gravity chain, which will process forwards_to_clear number of pending ibc auto forwards
pub async fn execute_pending_ibc_auto_forwards(
    contact: &Contact,
    cosmos_key: impl PrivateKey,
    fee: Coin,
    forwards_to_clear: u64,
) -> Result<(), CosmosGrpcError> {
    let prefix = contact.get_prefix();
    let cosmos_addr = cosmos_key.to_address(&prefix).unwrap();
    let msg = Msg::new(
        MSG_EXECUTE_IBC_AUTO_FORWARDS_TYPE_URL,
        MsgExecuteIbcAutoForwards {
            forwards_to_clear,
            executor: cosmos_addr.to_string(),
        },
    );
    let timeout = Duration::from_secs(60);
    let res = contact
        .send_message(&[msg], None, &[fee], Some(timeout), cosmos_key)
        .await;

    if res.is_err() {
        return Err(res.err().unwrap());
    }

    Ok(())
}
