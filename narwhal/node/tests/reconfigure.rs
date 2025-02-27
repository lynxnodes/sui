// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::mutable_key_type)]

use arc_swap::ArcSwap;
use bytes::Bytes;
use config::{Committee, Epoch, Parameters, SharedWorkerCache, WorkerCache, WorkerId};
use crypto::{KeyPair, NetworkKeyPair, PublicKey};
use executor::ExecutionState;
use fastcrypto::traits::KeyPair as _;
use futures::future::{join_all, try_join_all};
use mysten_metrics::RegistryService;
use narwhal_node as node;
use node::{restarter::NodeRestarter, Node};
use prometheus::Registry;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use storage::NodeStorage;
use test_utils::CommitteeFixture;
use tokio::{
    sync::mpsc::{channel, Receiver, Sender},
    time::{interval, sleep, Duration, MissedTickBehavior},
};
use tracing::info;
use types::{ConsensusOutput, Transaction};
use types::{ReconfigureNotification, TransactionProto, TransactionsClient};
use worker::TrivialTransactionValidator;

/// A simple/dumb execution engine.
struct SimpleExecutionState {
    keypair: KeyPair,
    network_keypair: NetworkKeyPair,
    worker_keypairs: Vec<NetworkKeyPair>,
    worker_cache: WorkerCache,
    committee: Arc<Mutex<Committee>>,
    tx_output: Sender<u64>,
    tx_reconfigure: Sender<(
        KeyPair,
        NetworkKeyPair,
        Committee,
        Vec<(WorkerId, NetworkKeyPair)>,
        WorkerCache,
    )>,
}

impl SimpleExecutionState {
    pub fn new(
        keypair: KeyPair,
        network_keypair: NetworkKeyPair,
        worker_keypairs: Vec<NetworkKeyPair>,
        worker_cache: WorkerCache,
        committee: Committee,
        tx_output: Sender<u64>,
        tx_reconfigure: Sender<(
            KeyPair,
            NetworkKeyPair,
            Committee,
            Vec<(WorkerId, NetworkKeyPair)>,
            WorkerCache,
        )>,
    ) -> Self {
        Self {
            keypair,
            network_keypair,
            worker_keypairs,
            worker_cache,
            committee: Arc::new(Mutex::new(committee)),
            tx_output,
            tx_reconfigure,
        }
    }
}

#[async_trait::async_trait]
impl ExecutionState for SimpleExecutionState {
    async fn handle_consensus_output(&self, consensus_output: ConsensusOutput) {
        if consensus_output.sub_dag.sub_dag_index % 3 == 0 {
            for (_, batches) in consensus_output.batches {
                for batch in batches {
                    for transaction in batch.transactions.into_iter() {
                        self.process_transaction(transaction, true).await;
                    }
                }
            }
        }
    }

    async fn last_executed_sub_dag_index(&self) -> u64 {
        0
    }
}

impl SimpleExecutionState {
    async fn process_transaction(&self, transaction: Transaction, change_epoch: bool) {
        let transaction: u64 = bincode::deserialize(&transaction).unwrap();
        // Change epoch every few certificates. Note that empty certificates are not provided to
        // this function (they are immediately skipped).
        let mut epoch = self.committee.lock().unwrap().epoch();
        if transaction >= epoch && change_epoch {
            epoch += 1;

            self.send_new_epoch_reconfigure(epoch).await;
        }

        let _ = self.tx_output.send(epoch).await;
    }

    async fn send_new_epoch_reconfigure(&self, epoch: Epoch) {
        {
            let mut guard = self.committee.lock().unwrap();
            guard.epoch = epoch;
        }

        let worker_ids_and_keypairs = self
            .worker_keypairs
            .iter()
            .enumerate()
            .map(|(i, k)| (i as WorkerId, k.copy()))
            .collect();

        let new_committee = self.committee.lock().unwrap().clone();

        self.tx_reconfigure
            .send((
                self.keypair.copy(),
                self.network_keypair.copy(),
                new_committee,
                worker_ids_and_keypairs,
                self.worker_cache.clone(),
            ))
            .await
            .unwrap();
    }
}

async fn run_client(
    name: PublicKey,
    worker_cache: SharedWorkerCache,
    mut rx_reconfigure: Receiver<u64>,
) {
    let target = worker_cache
        .load()
        .worker(&name, /* id */ &0)
        .expect("Our key or worker id is not in the worker cache")
        .transactions;
    let config = mysten_network::config::Config::new();
    let channel = config.connect_lazy(&target).unwrap();
    let mut client = TransactionsClient::new(channel);

    // Make a transaction to submit for ever.
    let mut tx = TransactionProto {
        transaction: Bytes::from(0u64.to_be_bytes().to_vec()),
    };

    // Repeatedly send transactions.
    let mut interval = interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::pin!(interval);

    loop {
        tokio::select! {
            // Wait a bit before repeating.
            _ = interval.tick() => {
                // Send a transactions.
                if client.submit_transaction(tx.clone()).await.is_err() {
                    // The workers are still down.
                    sleep(Duration::from_millis(100)).await;
                }
            },

            // Send transactions on the new epoch.
            Some(epoch) = rx_reconfigure.recv() => {
                tx = TransactionProto {
                    transaction: Bytes::from(epoch.to_le_bytes().to_vec()),
                };
            }
        }
    }
}

#[ignore]
#[tokio::test]
async fn restart() {
    telemetry_subscribers::init_for_testing();

    let fixture = CommitteeFixture::builder()
        .number_of_workers(NonZeroUsize::new(1).unwrap())
        .randomize_ports(true)
        .build();
    let committee = fixture.committee();
    let worker_cache = fixture.shared_worker_cache();

    // Spawn the nodes.
    let mut rx_nodes = Vec::new();
    let latest_observed_epoch = Arc::new(AtomicU64::new(0));

    let mut validators_execution_states = Vec::new();

    for a in fixture.authorities() {
        let (tx_output, rx_output) = channel(10);
        let (tx_node_reconfigure, rx_node_reconfigure) = channel(10);

        let execution_state = Arc::new(SimpleExecutionState::new(
            a.keypair().copy(),
            a.network_keypair().copy(),
            a.worker_keypairs(),
            fixture.worker_cache(),
            committee.clone(),
            tx_output,
            tx_node_reconfigure,
        ));

        validators_execution_states.push(execution_state.clone());

        let worker_ids_and_keypairs = a
            .worker_keypairs()
            .iter()
            .enumerate()
            .map(|(i, k)| (i as WorkerId, k.copy()))
            .collect();

        let committee = committee.clone();
        let worker_cache = worker_cache.clone();

        let parameters = Parameters {
            batch_size: 200,
            max_header_delay: Duration::from_secs(1),
            max_header_num_of_batches: 1,
            ..Parameters::default()
        };

        let register_service = RegistryService::new(Registry::new());

        let keypair = a.keypair().copy();
        let network_keypair = a.network_keypair().copy();
        tokio::spawn(async move {
            NodeRestarter::watch(
                keypair,
                network_keypair,
                worker_ids_and_keypairs,
                &committee,
                worker_cache,
                /* base_store_path */ test_utils::temp_dir(),
                execution_state,
                parameters,
                TrivialTransactionValidator::default(),
                rx_node_reconfigure,
                register_service,
            )
            .await;
        });

        rx_nodes.push(rx_output);
    }

    // Give a chance to the nodes to start.
    tokio::task::yield_now().await;

    // Spawn some clients.
    let mut tx_clients = Vec::new();
    for a in fixture.authorities() {
        let (tx_client_reconfigure, rx_client_reconfigure) = channel(10);
        tx_clients.push(tx_client_reconfigure);

        let name = a.public_key();
        let worker_cache = worker_cache.clone();
        tokio::spawn(
            async move { run_client(name, worker_cache.clone(), rx_client_reconfigure).await },
        );
    }

    // Listen to the outputs.
    let mut handles = Vec::new();
    for (tx, mut rx) in tx_clients.into_iter().zip(rx_nodes.into_iter()) {
        let global_epoch = latest_observed_epoch.clone();
        let execution_state = validators_execution_states.remove(0);

        handles.push(tokio::spawn(async move {
            let mut current_epoch = 0u64;
            static MAX_EPOCH: u64 = 10;

            // Repeatedly send transactions.
            let mut interval = interval(Duration::from_secs(3));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    result = rx.recv() => {
                        // channel closed, we won't be able to receive any further epoch updates so
                        // we need to exit the loop
                        if result.is_none() {
                            break;
                        }

                        let epoch = result.unwrap();

                        info!("Received epoch {}", epoch);

                        // update the latest observed global epoch - but only swap
                        // if it's greater than the previous value
                        let _ = global_epoch.compare_exchange(epoch-1, epoch, Ordering::SeqCst, Ordering::SeqCst);

                        if epoch == MAX_EPOCH {
                            return;
                        }
                        if epoch > current_epoch {
                            current_epoch = epoch;
                            tx.send(current_epoch).await.unwrap();
                        }
                    },
                    _ = interval.tick() => {
                        // detect whether all the other nodes managed to advance in epochs
                        // and our node did fall behind - in this case we want to advance
                        // our node
                        let global_epoch = global_epoch.load(Ordering::SeqCst);

                        if global_epoch > current_epoch {
                            info!("Detected greater epoch compared to our current {global_epoch} > {current_epoch} : will update epoch");

                            current_epoch = global_epoch;

                            // reconfigure - send details
                            execution_state.send_new_epoch_reconfigure(current_epoch).await;
                        }
                    }
                }
            }

            if current_epoch < MAX_EPOCH {
                panic!("Node never reached epoch {MAX_EPOCH}, something broke our connection");
            }
        }));
    }

    try_join_all(handles)
        .await
        .expect("No error should occurred");
}

#[ignore]
#[tokio::test]
async fn epoch_change() {
    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    let committee = fixture.committee();
    let worker_cache = fixture.shared_worker_cache();
    let parameters = fixture
        .authorities()
        .map(|a| {
            (
                a.public_key(),
                Parameters {
                    batch_size: 200,
                    header_num_of_batches_threshold: 1, // One batch digest
                    ..Parameters::default()
                },
            )
        })
        .collect::<HashMap<_, _>>();

    // Spawn the nodes.
    let mut rx_nodes = Vec::new();

    for a in fixture.authorities() {
        let (tx_output, rx_output) = channel(10);
        let (tx_node_reconfigure, mut rx_node_reconfigure) = channel(10);

        let name = a.public_key();
        let store = NodeStorage::reopen(test_utils::temp_dir());

        let execution_state = Arc::new(SimpleExecutionState::new(
            a.keypair().copy(),
            a.network_keypair().copy(),
            a.worker_keypairs(),
            fixture.worker_cache(),
            committee.clone(),
            tx_output,
            tx_node_reconfigure,
        ));

        // Start a task that will broadcast the committee change signal.
        let parameters_clone = parameters.get(&name).unwrap().clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();

            while let Some((_, _, committee, _, _)) = rx_node_reconfigure.recv().await {
                let message = ReconfigureNotification::NewEpoch(committee.clone());
                client
                    .post(format!(
                        "http://127.0.0.1:{}/reconfigure",
                        parameters_clone
                            .network_admin_server
                            .primary_network_admin_server_port
                    ))
                    .json(&message)
                    .send()
                    .await
                    .unwrap();
            }
        });

        let p = parameters.get(&name).unwrap().clone();
        let _primary_handles = Node::spawn_primary(
            a.keypair().copy(),
            a.network_keypair().copy(),
            Arc::new(ArcSwap::new(Arc::new(committee.clone()))),
            worker_cache.clone(),
            &store,
            p.clone(),
            /* consensus */ true,
            execution_state,
            &Registry::new(),
        )
        .await
        .unwrap();

        let _worker_handles = Node::spawn_workers(
            name,
            /* worker ids_and_keypairs */ vec![(0, a.worker(0).keypair().copy())],
            Arc::new(ArcSwap::new(Arc::new(committee.clone()))),
            worker_cache.clone(),
            &store,
            p,
            TrivialTransactionValidator::default(),
            &Registry::new(),
        );

        rx_nodes.push(rx_output);
    }

    // Give a chance to the nodes to start.
    tokio::task::yield_now().await;

    // Spawn some clients.
    let mut tx_clients = Vec::new();
    for a in fixture.authorities() {
        let (tx_client_reconfigure, rx_client_reconfigure) = channel(10);
        tx_clients.push(tx_client_reconfigure);

        let name = a.public_key();
        let worker_cache = worker_cache.clone();
        tokio::spawn(
            async move { run_client(name, worker_cache.clone(), rx_client_reconfigure).await },
        );
    }

    // Listen to the outputs.
    let mut handles = Vec::new();
    for (tx, mut rx) in tx_clients.into_iter().zip(rx_nodes.into_iter()) {
        handles.push(tokio::spawn(async move {
            let mut current_epoch = 0u64;
            while let Some(epoch) = rx.recv().await {
                if epoch == 5 {
                    return;
                }
                if epoch > current_epoch {
                    current_epoch = epoch;
                    tx.send(current_epoch).await.unwrap();
                }
            }
        }));
    }
    join_all(handles).await;
}
