// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use futures::future::join_all;
use move_core_types::ident_str;
use mysten_metrics::RegistryService;
use prometheus::Registry;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sui_config::{NodeConfig, ValidatorInfo};
use sui_core::authority_aggregator::{AuthAggMetrics, AuthorityAggregator};
use sui_core::consensus_adapter::position_submit_certificate;
use sui_core::safe_client::SafeClientMetricsBase;
use sui_core::signature_verifier::DefaultSignatureVerifier;
use sui_core::test_utils::make_transfer_sui_transaction;
use sui_macros::sim_test;
use sui_node::SuiNodeHandle;
use sui_types::base_types::SuiAddress;
use sui_types::crypto::ToFromBytes;
use sui_types::crypto::{generate_proof_of_possession, get_account_key_pair};
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message;
use sui_types::messages::{
    CallArg, CertifiedTransactionEffects, ExecutionStatus, ObjectArg, TransactionData,
    TransactionEffectsAPI, VerifiedTransaction,
};
use sui_types::object::Object;
use sui_types::utils::to_sender_signed_transaction;
use sui_types::{
    SUI_FRAMEWORK_OBJECT_ID, SUI_SYSTEM_STATE_OBJECT_ID, SUI_SYSTEM_STATE_OBJECT_SHARED_VERSION,
};
use test_utils::authority::{start_node, test_and_configure_authority_configs};
use test_utils::{
    authority::{spawn_test_authorities, test_authority_configs},
    network::TestClusterBuilder,
};
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

#[sim_test]
async fn advance_epoch_tx_test() {
    let authorities = spawn_test_authorities([].into_iter(), &test_authority_configs()).await;
    let states: Vec<_> = authorities
        .iter()
        .map(|authority| authority.with(|node| node.state()))
        .collect();
    let tasks: Vec<_> = states
        .iter()
        .map(|state| async {
            let (_system_state, effects) = state
                .create_and_execute_advance_epoch_tx(
                    &state.epoch_store_for_testing(),
                    &GasCostSummary::new(0, 0, 0),
                    0, // checkpoint
                    0, // epoch_start_timestamp_ms
                )
                .await
                .unwrap();
            // Check that the validator didn't commit the transaction yet.
            assert!(state
                .get_signed_effects_and_maybe_resign(
                    effects.transaction_digest(),
                    &state.epoch_store_for_testing()
                )
                .unwrap()
                .is_none());
            effects
        })
        .collect();
    let results: HashSet<_> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|result| result.digest())
        .collect();
    // Check that all validators have the same result.
    assert_eq!(results.len(), 1);
}

#[sim_test]
async fn basic_reconfig_end_to_end_test() {
    // TODO remove this sleep when this test passes consistently
    sleep(Duration::from_secs(1)).await;
    let authorities = spawn_test_authorities([].into_iter(), &test_authority_configs()).await;
    trigger_reconfiguration(&authorities).await;
}

#[sim_test]
async fn reconfig_with_revert_end_to_end_test() {
    let (sender, keypair) = get_account_key_pair();
    let gas1 = Object::with_owner_for_testing(sender); // committed
    let gas2 = Object::with_owner_for_testing(sender); // (most likely) reverted
    let authorities = spawn_test_authorities(
        [gas1.clone(), gas2.clone()].into_iter(),
        &test_authority_configs(),
    )
    .await;
    let registry = Registry::new();

    // gas1 transaction is committed
    let tx = make_transfer_sui_transaction(
        gas1.compute_object_reference(),
        sender,
        None,
        sender,
        &keypair,
        None,
    );
    let net = AuthorityAggregator::<_, DefaultSignatureVerifier>::new_from_local_system_state(
        &authorities[0].with(|node| node.state().db()),
        &authorities[0].with(|node| node.state().committee_store().clone()),
        SafeClientMetricsBase::new(&registry),
        AuthAggMetrics::new(&registry),
    )
    .unwrap();
    let cert = net
        .process_transaction(tx.clone())
        .await
        .unwrap()
        .into_cert_for_testing();
    let (effects1, _) = net
        .process_certificate(cert.clone().into_inner())
        .await
        .unwrap();
    assert_eq!(0, effects1.epoch());

    // gas2 transaction is (most likely) reverted
    let tx = make_transfer_sui_transaction(
        gas2.compute_object_reference(),
        sender,
        None,
        sender,
        &keypair,
        None,
    );
    let cert = net
        .process_transaction(tx.clone())
        .await
        .unwrap()
        .into_cert_for_testing();

    // Close epoch on 3 (2f+1) validators.
    let mut reverting_authority_idx = None;
    for (i, handle) in authorities.iter().enumerate() {
        handle
            .with_async(|node| async {
                if position_submit_certificate(&net.committee, &node.state().name, tx.digest())
                    < (authorities.len() - 1)
                {
                    node.close_epoch_for_testing().await.unwrap();
                } else {
                    // remember the authority that wouild submit it to consensus last.
                    reverting_authority_idx = Some(i);
                }
            })
            .await;
    }

    let reverting_authority_idx = reverting_authority_idx.unwrap();
    let client = net
        .get_client(&authorities[reverting_authority_idx].with(|node| node.state().name))
        .unwrap();
    client
        .handle_certificate(cert.clone().into_inner())
        .await
        .unwrap();

    authorities[reverting_authority_idx]
        .with_async(|node| async {
            let object = node
                .state()
                .get_objects(&[gas2.id()])
                .await
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .unwrap();
            // verify that authority 0 advanced object version
            assert_eq!(2, object.version().value());
        })
        .await;

    // Wait for all nodes to reach the next epoch.
    let handles: Vec<_> = authorities
        .iter()
        .map(|handle| {
            handle.with_async(|node| async {
                loop {
                    if node.state().current_epoch_for_testing() == 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            })
        })
        .collect();
    join_all(handles).await;

    let mut epoch = None;
    for handle in authorities.iter() {
        handle
            .with_async(|node| async {
                let object = node
                    .state()
                    .get_objects(&[gas1.id()])
                    .await
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .unwrap();
                assert_eq!(2, object.version().value());
                // Due to race conditions, it's possible that tx2 went in
                // before 2f+1 validators sent EndOfPublish messages and close
                // the curtain of epoch 0. So, we are asserting that
                // the object version is either 1 or 2, but needs to be
                // consistent in all validators.
                // Note that previously test checked that object version == 2 on authority 0
                let object = node
                    .state()
                    .get_objects(&[gas2.id()])
                    .await
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .unwrap();
                let object_version = object.version().value();
                if epoch.is_none() {
                    assert!(object_version == 1 || object_version == 2);
                    epoch.replace(object_version);
                } else {
                    assert_eq!(epoch, Some(object_version));
                }
            })
            .await;
    }
}

// This test just starts up a cluster that reconfigures itself under 0 load.
#[sim_test]
async fn test_passive_reconfig() {
    telemetry_subscribers::init_for_testing();
    sui_protocol_config::ProtocolConfig::poison_get_for_min_version();

    let test_cluster = TestClusterBuilder::new()
        .with_epoch_duration_ms(1000)
        .build()
        .await
        .unwrap();

    let mut epoch_rx = test_cluster
        .fullnode_handle
        .sui_node
        .subscribe_to_epoch_change();

    let target_epoch: u64 = std::env::var("RECONFIG_TARGET_EPOCH")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(4);

    timeout(Duration::from_secs(60), async move {
        while let Ok((committee, _)) = epoch_rx.recv().await {
            info!("received epoch {}", committee.epoch());
            if committee.epoch() >= target_epoch {
                break;
            }
        }
    })
    .await
    .expect("Timed out waiting for cluster to target epoch");
}

#[sim_test]
async fn test_validator_resign_effects() {
    // This test checks that validators are able to re-sign transaction effects that were finalized
    // in previous epochs. This allows authority aggregator to form a new effects certificate
    // in the new epoch.
    let (sender, keypair) = get_account_key_pair();
    let gas = Object::with_owner_for_testing(sender);
    let configs = test_authority_configs();
    let authorities = spawn_test_authorities([gas.clone()].into_iter(), &configs).await;
    let tx = make_transfer_sui_transaction(
        gas.compute_object_reference(),
        sender,
        None,
        sender,
        &keypair,
        None,
    );
    let registry = Registry::new();
    let mut net = AuthorityAggregator::<_, DefaultSignatureVerifier>::new_from_local_system_state(
        &authorities[0].with(|node| node.state().db()),
        &authorities[0].with(|node| node.state().committee_store().clone()),
        SafeClientMetricsBase::new(&registry),
        AuthAggMetrics::new(&registry),
    )
    .unwrap();
    let cert = net
        .process_transaction(tx.clone())
        .await
        .unwrap()
        .into_cert_for_testing();
    let (effects0, _) = net
        .process_certificate(cert.clone().into_inner())
        .await
        .unwrap();
    assert_eq!(effects0.epoch(), 0);
    // Give it enough time for the transaction to be checkpointed and hence finalized.
    sleep(Duration::from_secs(10)).await;
    trigger_reconfiguration(&authorities).await;
    // Manually reconfigure the aggregator.
    net.committee.epoch = 1;
    let (effects1, _) = net.process_certificate(cert.into_inner()).await.unwrap();
    // Ensure that we are able to form a new effects cert in the new epoch.
    assert_eq!(effects1.epoch(), 1);
    assert_eq!(effects0.into_message(), effects1.into_message());
}

#[sim_test]
async fn test_reconfig_with_committee_change_basic() {
    // This test exercise the full flow of a validator joining the network, catch up and then leave.

    // TODO: In order to better "test" this flow we probably want to set the validators to ignore
    // all p2p peer connections so that we can verify that new nodes joining can really "talk" with the
    // other validators in the set.
    let init_configs = test_and_configure_authority_configs(4);

    // Generate a new validator config.
    // Our committee generation uses a fixed seed, so we need to generate a new committee
    // with one extra validator.
    // Furthermore, since the order is not fixed, we need to find the new validator
    // that doesn't exist in the previous committee manually.
    // The order of validator_set() and validator_configs() is also different.
    // TODO: We should really fix the above inconveniences.
    let public_keys: HashSet<_> = init_configs
        .validator_set()
        .iter()
        .map(|v| v.protocol_key())
        .collect();
    let new_configs = test_and_configure_authority_configs(5);
    let new_validator = new_configs
        .validator_set()
        .into_iter()
        .find(|v| !public_keys.contains(&v.protocol_key()))
        .unwrap();
    let new_node_config = new_configs
        .validator_configs()
        .iter()
        .find(|v| !public_keys.contains(&v.protocol_public_key()))
        .unwrap();
    info!(
        "New validator is: {:?}",
        new_validator.protocol_key.concise()
    );

    let sender = new_node_config.sui_address();
    let gas = Object::with_owner_for_testing(sender);
    let stake = Object::with_owner_for_testing(sender);

    let mut authorities =
        spawn_test_authorities([gas.clone(), stake.clone()].into_iter(), &init_configs).await;

    let transaction =
        create_join_committee_tx(&gas, &stake, &new_validator, new_node_config, sender);
    let effects = execute_transaction(&authorities, transaction)
        .await
        .unwrap();
    assert!(effects.status().is_ok());

    trigger_reconfiguration(&authorities).await;
    // Check that a new validator has joined the committee.
    authorities[0].with(|node| {
        assert_eq!(
            node.state()
                .epoch_store_for_testing()
                .committee()
                .num_members(),
            5
        );
    });

    let mut new_node_config_clone = new_node_config.clone();
    // Make sure that the new validator config shares the same genesis as the initial one.
    new_node_config_clone.genesis = init_configs.validator_configs[0].genesis.clone();
    let handle = start_node(
        &new_node_config_clone,
        RegistryService::new(Registry::new()),
    )
    .await;
    // We have to manually insert the genesis objects since the test utility doesn't.
    handle
        .with_async(|node| async {
            node.state().insert_genesis_object(stake.clone()).await;
            node.state().insert_genesis_object(gas.clone()).await;
            // When the node started, it's not part of the committee, and hence a fullnode.
            assert!(node
                .state()
                .is_fullnode(&node.state().epoch_store_for_testing()));
        })
        .await;
    // Give the new validator enough time to catch up and sync.
    tokio::time::sleep(Duration::from_secs(30)).await;
    handle.with(|node| {
        let latest_checkpoint = node
            .state()
            .get_latest_checkpoint_sequence_number()
            .unwrap();
        // Eventually the validator will catch up to the new epoch and become part of the committee.
        assert!(latest_checkpoint > 10);
        assert!(node
            .state()
            .is_validator(&node.state().epoch_store_for_testing()));
    });

    let gas = authorities[0].with(|node| node.state().db().get_object(&gas.id()).unwrap().unwrap());
    let transaction = create_leave_committee_tx(&gas, sender, new_node_config);
    let effects = execute_transaction(&authorities, transaction)
        .await
        .unwrap();
    assert!(effects.status().is_ok());

    authorities.push(handle);
    trigger_reconfiguration(&authorities).await;

    // Check that this validator has left the committee, and is no longer a validator.
    authorities[4].with(|node| {
        assert_eq!(
            node.state()
                .epoch_store_for_testing()
                .committee()
                .num_members(),
            4
        );
        assert!(node
            .state()
            .is_fullnode(&node.state().epoch_store_for_testing()));
    })
}

#[sim_test]
async fn test_reconfig_with_committee_change_stress() {
    // Network config composed of the join of all committees that will
    // exist at any point during this test. Each NetworkConfig epoch committee
    // will be a subset of this
    let validator_superset = test_and_configure_authority_configs(9);

    // This needs to be written to genesis for all validators, present and future
    // (either that or we create these objects via Transaction later, but that's more work)
    let object_set: HashMap<_, _> = validator_superset
        .validator_configs()
        .iter()
        .map(|config| {
            let sender = config.sui_address();
            let gas = Object::with_owner_for_testing(sender);
            let stake = Object::with_owner_for_testing(sender);
            (sender, (gas, stake))
        })
        .collect();
    let flattened_object_set: Vec<_> = object_set
        .values()
        .flat_map(|(g, s)| vec![g.clone(), s.clone()])
        .collect();

    // initial committee
    let current_network_config = test_and_configure_authority_configs(7);

    let mut authorities =
        spawn_test_authorities(flattened_object_set.clone(), &current_network_config).await;

    let public_keys: HashSet<_> = current_network_config
        .validator_set()
        .into_iter()
        .map(|v| v.protocol_key())
        .collect();

    let mut current_set: Vec<_> = current_network_config
        .validator_set()
        .into_iter()
        .map(|v| {
            let node_config = current_network_config
                .validator_configs()
                .iter()
                .find(|c| c.protocol_public_key() == v.protocol_key())
                .unwrap();
            (v, node_config)
        })
        .collect();
    assert_eq!(current_set.len(), 7);

    let other_validators: Vec<_> = validator_superset
        .validator_set()
        .into_iter()
        .filter(|v| !public_keys.contains(&v.protocol_key()))
        .collect();
    let mut standby_set: Vec<_> = other_validators
        .clone()
        .into_iter()
        .map(|v| {
            let node_config = validator_superset
                .validator_configs()
                .iter()
                .find(|c| c.protocol_public_key() == v.protocol_key())
                .unwrap();
            (v, node_config)
        })
        .collect();
    assert_eq!(standby_set.len(), 2);

    // circulate the committee 3 times (i.e. 3 epochs). By the end, we will have cycled through
    // nearly the full validator set
    for _ in 0..3 {
        // Add first 2 validators from standby_set to committee (and add to end of current_set).
        // Remove first 2 validators from committee (and beginning of current_set) and add to end of standby_set.
        // This effectively creates a circular buffer of two different leaving and joining
        // validators per epoch

        // adding new validators
        let joining_validators: Vec<_> = standby_set.drain(..2).collect();
        for (val, node_config) in joining_validators.clone() {
            let sender = node_config.sui_address();
            let (gas, stake) = object_set.get(&sender).unwrap();
            let transaction = create_join_committee_tx(gas, stake, &val, node_config, sender);
            let effects = execute_transaction(&authorities, transaction)
                .await
                .unwrap();

            // TODO(william)
            if let ExecutionStatus::Failure {
                error: e,
                command: c,
            } = effects.status.clone()
            {
                panic!("Transaction unsuccessful: {:?}, {:?}", e, c);
            } else {
                info!("Add validator tx successful");
            }
            // assert!(effects.status.is_ok());

            // update our working set for validation after reconfig
            current_set.push((val.clone(), node_config));
        }

        // removing old validators
        let leaving_validators: Vec<_> = current_set.drain(..2).collect();
        for (val, node_config) in &leaving_validators {
            let sender = node_config.sui_address();
            let (gas, _stake) = object_set.get(&sender).unwrap();
            let transaction = create_leave_committee_tx(gas, sender, node_config);
            let effects = execute_transaction(&authorities, transaction)
                .await
                .unwrap();
            assert!(effects.status.is_ok());

            // update our standby set for validation after reconfig
            standby_set.push((val.clone(), node_config));
        }

        trigger_reconfiguration(&authorities).await;

        // Check that a new validator has joined the committee.
        assert_eq!(current_set.len(), 7);
        authorities[0].with(|node| {
            assert_eq!(
                node.state()
                    .epoch_store_for_testing()
                    .committee()
                    .num_members(),
                7
            );
            for (val, _) in current_set.iter() {
                assert!(node
                    .state()
                    .epoch_store_for_testing()
                    .committee()
                    .authority_exists(&val.protocol_key()));
            }
        });

        // Start new nodes as validators and allow time to catch up
        let mut new_handles: Vec<_> = vec![];
        for (_val, node_config) in joining_validators.clone() {
            let mut node_config_clone = node_config.clone();
            // Make sure that the new validator config shares the same genesis as the initial one.
            node_config_clone.genesis = current_network_config.validator_configs[0].genesis.clone();
            let handle =
                start_node(&node_config_clone, RegistryService::new(Registry::new())).await;
            // We have to manually insert the genesis objects since the test utility doesn't.
            handle
                .with_async(|node| async {
                    for obj in flattened_object_set.clone() {
                        node.state().insert_genesis_object(obj.clone()).await;
                    }
                    // When the node started, it's not part of the committee, and hence a fullnode.
                    assert!(node
                        .state()
                        .is_fullnode(&node.state().epoch_store_for_testing()));
                })
                .await;
            new_handles.push(handle);
        }

        // Give the new validator enough time to catch up and sync.
        tokio::time::sleep(Duration::from_secs(30)).await;

        for handle in new_handles {
            handle.with(|node| {
                let latest_checkpoint = node
                    .state()
                    .get_latest_checkpoint_sequence_number()
                    .unwrap();
                // Eventually the validator will catch up to the new epoch and become part of the committee.
                assert!(latest_checkpoint > 10);
                assert!(node
                    .state()
                    .is_validator(&node.state().epoch_store_for_testing()));
            });
            authorities.push(handle);
        }

        let left_validator_pubkeys = leaving_validators
            .iter()
            .map(|(val, _)| val.protocol_key())
            .collect::<Vec<_>>();

        let (authorities, removed_authorities): (Vec<_>, Vec<_>) =
            authorities.iter().partition(|authority| {
                authority.with(|node| left_validator_pubkeys.contains(&node.state().name))
            });

        // Check that this validator has left the committee, and is no longer a validator.
        assert_eq!(authorities.len(), 7);
        for r in removed_authorities.iter() {
            assert!(r.with(|node| node
                .state()
                .is_fullnode(&node.state().epoch_store_for_testing())));
        }

        // check new committee size
        authorities[0].with(|node| {
            assert_eq!(
                node.state()
                    .epoch_store_for_testing()
                    .committee()
                    .num_members(),
                7
            )
        });
    }
}

fn create_join_committee_tx(
    gas: &Object,
    stake: &Object,
    val: &ValidatorInfo,
    node_config: &NodeConfig,
    sender: SuiAddress,
) -> VerifiedTransaction {
    let proof_of_possession = generate_proof_of_possession(node_config.protocol_key_pair(), sender);
    let tx_data = TransactionData::new_move_call_with_dummy_gas_price(
        sender,
        SUI_FRAMEWORK_OBJECT_ID,
        ident_str!("sui_system").to_owned(),
        ident_str!("request_add_validator").to_owned(),
        vec![],
        gas.compute_object_reference(),
        vec![
            CallArg::Object(ObjectArg::SharedObject {
                id: SUI_SYSTEM_STATE_OBJECT_ID,
                initial_shared_version: SUI_SYSTEM_STATE_OBJECT_SHARED_VERSION,
                mutable: true,
            }),
            CallArg::Pure(bcs::to_bytes(&val.protocol_key().as_bytes().to_vec()).unwrap()),
            CallArg::Pure(bcs::to_bytes(val.network_key().as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes(val.worker_key().as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes(proof_of_possession.as_ref()).unwrap()),
            CallArg::Pure(bcs::to_bytes("name".as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes("description".as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes("image_url".as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes("project_url".as_bytes()).unwrap()),
            CallArg::Pure(bcs::to_bytes(&val.network_address().to_vec()).unwrap()),
            CallArg::Pure(bcs::to_bytes(&val.p2p_address().to_vec()).unwrap()),
            CallArg::Pure(bcs::to_bytes(&val.narwhal_primary_address().to_vec()).unwrap()),
            CallArg::Pure(bcs::to_bytes(&val.narwhal_worker_address().to_vec()).unwrap()),
            CallArg::Object(ObjectArg::ImmOrOwnedObject(
                stake.compute_object_reference(),
            )),
            CallArg::Pure(bcs::to_bytes(&1u64).unwrap()), // gas_price
            CallArg::Pure(bcs::to_bytes(&0u64).unwrap()), // commission_rate
        ],
        10000,
    );
    to_sender_signed_transaction(tx_data, node_config.account_key_pair())
}

fn create_leave_committee_tx(
    gas: &Object,
    sender: SuiAddress,
    node_config: &NodeConfig,
) -> VerifiedTransaction {
    let tx_data = TransactionData::new_move_call_with_dummy_gas_price(
        sender,
        SUI_FRAMEWORK_OBJECT_ID,
        ident_str!("sui_system").to_owned(),
        ident_str!("request_remove_validator").to_owned(),
        vec![],
        gas.compute_object_reference(),
        vec![CallArg::Object(ObjectArg::SharedObject {
            id: SUI_SYSTEM_STATE_OBJECT_ID,
            initial_shared_version: SUI_SYSTEM_STATE_OBJECT_SHARED_VERSION,
            mutable: true,
        })],
        10000,
    );
    to_sender_signed_transaction(tx_data, node_config.account_key_pair())
}

async fn trigger_reconfiguration(authorities: &[SuiNodeHandle]) {
    info!("Starting reconfiguration");
    let start = Instant::now();

    // Close epoch on 2f+1 validators.
    let cur_committee =
        authorities[0].with(|node| node.state().epoch_store_for_testing().committee().clone());
    let mut cur_stake = 0;
    for handle in authorities {
        handle
            .with_async(|node| async {
                node.close_epoch_for_testing().await.unwrap();
                cur_stake += cur_committee.weight(&node.state().name);
            })
            .await;
        if cur_stake >= cur_committee.quorum_threshold() {
            break;
        }
    }
    info!("close_epoch complete after {:?}", start.elapsed());

    // Wait for all nodes to reach the next epoch.
    let handles: Vec<_> = authorities
        .iter()
        .map(|handle| {
            handle.with_async(|node| async {
                let mut retries = 0;
                loop {
                    if node.state().epoch_store_for_testing().epoch() == cur_committee.epoch + 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    retries += 1;
                    if retries % 5 == 0 {
                        warn!(validator=?node.state().name.concise(), "Waiting for {:?} seconds for epoch change", retries);
                    }
                }
            })
        })
        .collect();

    timeout(Duration::from_secs(40), join_all(handles))
        .await
        .expect("timed out waiting for reconfiguration to complete");

    info!("reconfiguration complete after {:?}", start.elapsed());
}

async fn execute_transaction(
    authorities: &[SuiNodeHandle],
    transaction: VerifiedTransaction,
) -> anyhow::Result<CertifiedTransactionEffects> {
    let registry = Registry::new();
    let net = AuthorityAggregator::<_, DefaultSignatureVerifier>::new_from_local_system_state(
        &authorities[0].with(|node| node.state().db()),
        &authorities[0].with(|node| node.state().committee_store().clone()),
        SafeClientMetricsBase::new(&registry),
        AuthAggMetrics::new(&registry),
    )
    .unwrap();
    net.execute_transaction(&transaction)
        .await
        .map(|e| e.into_inner())
}
