mod fake_sync_behaviour;

use std::{collections::HashMap, time::Duration};

use futures::{
    channel::{mpsc, oneshot},
    StreamExt,
};
use libp2p::{identity, swarm::SwarmEvent, Multiaddr, PeerId, Swarm};
use spectrum_network::{
    network_controller::{NetworkController, NetworkControllerIn, NetworkControllerOut, NetworkMailbox},
    peer_conn_handler::{ConnHandlerError, PeerConnHandlerConf},
    peer_manager::{
        data::{ConnectionLossReason, PeerDestination, ReputationChange},
        peers_state::PeerRepo,
        NetworkingConfig, PeerManager, PeerManagerConfig, PeersMailbox,
    },
    protocol::{ProtocolConfig, ProtocolSpec, SYNC_PROTOCOL_ID},
    protocol_api::ProtocolMailbox,
    protocol_handler::{
        sync::{message::SyncSpec, NodeStatus, SyncBehaviour},
        MalformedMessage, ProtocolBehaviour, ProtocolHandler,
    },
    types::Reputation,
};

use crate::integration_tests::fake_sync_behaviour::FakeSyncBehaviour;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Peer {
    First,
    Second,
}

/// Integration test which covers:
///  - peer connection
///  - peer disconnection by sudden shutdown (`ResetByPeer`)
///  - peer punishment due to no-response
#[cfg_attr(feature = "test_peer_punish_too_slow", ignore)]
#[async_std::test]
async fn integration_test_0() {
    //               --------             --------
    // ?? <~~~~~~~~ | peer_0 | <~~~~~~~~ | peer_1 |
    //               --------             --------
    //
    // In this scenario `peer_0` has a non-existent peer in the bootstrap-peer set and `peer_1` has
    // only `peer_0` as a bootstrap peer.
    //   - `peer_1` will successfully establish a connection with `peer_0`
    //   - `peer_0`s attempted connection will trigger peer-punishment
    //   - Afterwards we shutdown `peer_1` and check for peer disconnection event in `peer_0`.
    let local_key_0 = identity::Keypair::generate_ed25519();
    let local_peer_id_0 = PeerId::from(local_key_0.public());
    let local_key_1 = identity::Keypair::generate_ed25519();
    let local_peer_id_1 = PeerId::from(local_key_1.public());

    // Non-existent peer
    let fake_peer_id = PeerId::random();
    let fake_addr: Multiaddr = "/ip4/127.0.0.1/tcp/1236".parse().unwrap();

    let addr_0: Multiaddr = "/ip4/127.0.0.1/tcp/1234".parse().unwrap();
    let addr_1: Multiaddr = "/ip4/127.0.0.1/tcp/1235".parse().unwrap();
    let peers_0 = vec![PeerDestination::PeerIdWithAddr(fake_peer_id, fake_addr)];
    let peers_1 = vec![PeerDestination::PeerIdWithAddr(local_peer_id_0, addr_0.clone())];

    let local_status_0 = NodeStatus {
        supported_protocols: Vec::from([SYNC_PROTOCOL_ID]),
        height: 0,
    };
    let local_status_1 = local_status_0.clone();
    let sync_behaviour_0 = |p| SyncBehaviour::new(p, local_status_0);
    let sync_behaviour_1 = |p| SyncBehaviour::new(p, local_status_1);

    let (nc_out_tx, mut nc_out_rx) = mpsc::channel(10);
    let (mut sync_handler_0, nc_0) = make_swarm_components(peers_0, sync_behaviour_0, 10);
    let (mut sync_handler_1, nc_1) = make_swarm_components(peers_1, sync_behaviour_1, 10);

    let sync_handler_0_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_0.select_next_some().await;
            dbg!(msg);
        }
    });

    let sync_handler_1_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_1.select_next_some().await;
            dbg!(msg);
        }
    });

    let (abortable_peer_0, handle_0) = futures::future::abortable(create_swarm(
        local_key_0,
        nc_0,
        addr_0,
        Peer::First,
        nc_out_tx.clone(),
    ));
    let (abortable_peer_1, handle_1) =
        futures::future::abortable(create_swarm(local_key_1, nc_1, addr_1, Peer::Second, nc_out_tx));
    let (cancel_tx_0, cancel_rx_0) = oneshot::channel::<()>();
    let (cancel_tx_1, cancel_rx_1) = oneshot::channel::<()>();

    // Spawn tasks for peer_0
    async_std::task::spawn(async move {
        let _ = cancel_rx_0.await;
        handle_0.abort();
        sync_handler_0_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(5)).await.unwrap();
        cancel_tx_0.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_0);

    // Spawn tasks for peer_1
    async_std::task::spawn(async move {
        let _ = cancel_rx_1.await;
        handle_1.abort();
        sync_handler_1_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(4)).await.unwrap();
        cancel_tx_1.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_1);

    // Collect messages from the peers. Note that the while loop below will end since
    // `abortable_peer_0` and `abortable_peer_1` is guaranteed to drop, leading to the senders
    // dropping too.
    let mut res_peer_0 = vec![];
    let mut res_peer_1 = vec![];
    while let Some((peer, nc_msg)) = nc_out_rx.next().await {
        match peer {
            Peer::First => res_peer_0.push(nc_msg),
            Peer::Second => res_peer_1.push(nc_msg),
        }
    }

    dbg!(&res_peer_0);
    dbg!(&res_peer_1);
    assert_eq!(
        NetworkControllerOut::PeerPunished {
            peer_id: fake_peer_id,
            reason: ReputationChange::NoResponse,
        },
        res_peer_0[0]
    );
    assert_eq!(
        NetworkControllerOut::ConnectedWithInboundPeer(local_peer_id_1),
        res_peer_0[1]
    );
    assert_eq!(
        Some(&NetworkControllerOut::Disconnected {
            peer_id: local_peer_id_1,
            reason: ConnectionLossReason::ResetByPeer,
        }),
        res_peer_0.last()
    );
    assert_eq!(
        Some(&NetworkControllerOut::ConnectedWithOutboundPeer(local_peer_id_0)),
        res_peer_1.first()
    );
}

/// Integration test which covers:
///  - peer connection
///  - peer punishment due to malformed message
///  - peer disconnection from reputation being too low
#[cfg_attr(feature = "test_peer_punish_too_slow", ignore)]
#[async_std::test]
async fn integration_test_1() {
    //   --------             --------
    //  | peer_0 | <~~~~~~~~ | peer_1 |
    //   --------             --------
    //
    // In this scenario `peer_0` has no bootstrap peers and `peer_1` has only `peer_0` as a
    // bootstrap peer. `peer_0` is running the Sync protocol and `peer_1` a fake-Sync protocol.
    // After `peer_1` establishes a connection to `peer_0`, `peer_1` will send a message which is
    // regarded as malformed by `peer_0`. `peer_0` then punishes `peer_1` and a disconnection is
    // triggered due to reputation being too low.
    let local_key_0 = identity::Keypair::generate_ed25519();
    let local_peer_id_0 = PeerId::from(local_key_0.public());
    let local_key_1 = identity::Keypair::generate_ed25519();
    let local_peer_id_1 = PeerId::from(local_key_1.public());

    let addr_0: Multiaddr = "/ip4/127.0.0.1/tcp/1237".parse().unwrap();
    let addr_1: Multiaddr = "/ip4/127.0.0.1/tcp/1238".parse().unwrap();
    let peers_0 = vec![];
    let peers_1 = vec![PeerDestination::PeerIdWithAddr(local_peer_id_0, addr_0.clone())];

    let local_status_0 = NodeStatus {
        supported_protocols: Vec::from([SYNC_PROTOCOL_ID]),
        height: 0,
    };
    let local_status_1 = local_status_0.clone();
    let sync_behaviour_0 = |p| SyncBehaviour::new(p, local_status_0);
    let fake_sync_behaviour = |p| FakeSyncBehaviour::new(p, local_status_1);

    let (nc_out_tx, mut nc_out_rx) = mpsc::channel(10);
    let (mut sync_handler_0, nc_0) = make_swarm_components(peers_0, sync_behaviour_0, 10);
    let (mut sync_handler_1, nc_1) = make_swarm_components(peers_1, fake_sync_behaviour, 10);
    let sync_handler_0_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_0.select_next_some().await;
            dbg!(msg);
        }
    });

    let sync_handler_1_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_1.select_next_some().await;
            dbg!(msg);
        }
    });

    let (abortable_peer_0, handle_0) = futures::future::abortable(create_swarm(
        local_key_0,
        nc_0,
        addr_0,
        Peer::First,
        nc_out_tx.clone(),
    ));
    let (abortable_peer_1, handle_1) =
        futures::future::abortable(create_swarm(local_key_1, nc_1, addr_1, Peer::Second, nc_out_tx));

    let (cancel_tx_0, cancel_rx_0) = oneshot::channel::<()>();
    let (cancel_tx_1, cancel_rx_1) = oneshot::channel::<()>();

    // Spawn tasks for peer_0
    async_std::task::spawn(async move {
        let _ = cancel_rx_0.await;
        handle_0.abort();
        sync_handler_0_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(6)).await.unwrap();
        cancel_tx_0.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_0);

    // Spawn tasks for peer_1
    async_std::task::spawn(async move {
        let _ = cancel_rx_1.await;
        handle_1.abort();
        sync_handler_1_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(5)).await.unwrap();
        cancel_tx_1.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_1);

    // Collect messages from the peers. Note that the while loop below will end since
    // `abortable_peer_0` and `abortable_peer_1` is guaranteed to drop, leading to the senders
    // dropping too.
    let mut res_peer_0 = vec![];
    let mut res_peer_1 = vec![];
    while let Some((peer, nc_msg)) = nc_out_rx.next().await {
        match peer {
            Peer::First => res_peer_0.push(nc_msg),
            Peer::Second => res_peer_1.push(nc_msg),
        }
    }

    dbg!(&res_peer_0);
    dbg!(&res_peer_1);

    assert_eq!(
        NetworkControllerOut::ConnectedWithInboundPeer(local_peer_id_1),
        res_peer_0[0]
    );
    assert_eq!(
        NetworkControllerOut::PeerPunished {
            peer_id: local_peer_id_1,
            reason: ReputationChange::MalformedMessage(MalformedMessage::UnknownFormat),
        },
        res_peer_0[1]
    );
    assert_eq!(
        Some(&NetworkControllerOut::Disconnected {
            peer_id: local_peer_id_1,
            reason: ConnectionLossReason::Reset(ConnHandlerError::UnacceptablePeer),
        }),
        res_peer_0.last()
    );
    assert_eq!(
        Some(&NetworkControllerOut::ConnectedWithOutboundPeer(local_peer_id_0)),
        res_peer_1.first()
    );
}

#[async_std::test]
#[cfg_attr(not(feature = "test_peer_punish_too_slow"), ignore)]
async fn integration_test_peer_punish_too_slow() {
    //   --------             --------
    //  | peer_0 | <~~~~~~~~ | peer_1 |
    //   --------             --------
    //
    // In this scenario `peer_0` has no bootstrap peers and `peer_1` has only `peer_0` as a
    // bootstrap peer.  After `peer_1` establishes a connection to `peer_0`, each peer will send
    // multiple `GetPeers` messages in order to saturate the message buffers of each peer, resulting
    // in peer disconnection.
    let local_key_0 = identity::Keypair::generate_ed25519();
    let local_peer_id_0 = PeerId::from(local_key_0.public());
    let local_key_1 = identity::Keypair::generate_ed25519();
    let local_peer_id_1 = PeerId::from(local_key_1.public());

    let addr_0: Multiaddr = "/ip4/127.0.0.1/tcp/1237".parse().unwrap();
    let addr_1: Multiaddr = "/ip4/127.0.0.1/tcp/1238".parse().unwrap();
    let peers_0 = vec![];
    let peers_1 = vec![PeerDestination::PeerIdWithAddr(local_peer_id_0, addr_0.clone())];

    let local_status_0 = NodeStatus {
        supported_protocols: Vec::from([SYNC_PROTOCOL_ID]),
        height: 0,
    };
    let local_status_1 = local_status_0.clone();
    let sync_behaviour_0 = |p| SyncBehaviour::new(p, local_status_0);
    let sync_behaviour_1 = |p| SyncBehaviour::new(p, local_status_1);

    let (nc_out_tx, mut nc_out_rx) = mpsc::channel(10);

    // It's crucial to have a buffer of size 1 for this test
    let msg_buffer_size = 1;
    let (mut sync_handler_0, nc_0) = make_swarm_components(peers_0, sync_behaviour_0, msg_buffer_size);
    let (mut sync_handler_1, nc_1) = make_swarm_components(peers_1, sync_behaviour_1, msg_buffer_size);
    let sync_handler_0_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_0.select_next_some().await;
            dbg!(msg);
        }
    });

    let sync_handler_1_handle = async_std::task::spawn(async move {
        loop {
            let msg = sync_handler_1.select_next_some().await;
            dbg!(msg);
        }
    });

    let (abortable_peer_0, handle_0) = futures::future::abortable(create_swarm(
        local_key_0,
        nc_0,
        addr_0,
        Peer::First,
        nc_out_tx.clone(),
    ));
    let (abortable_peer_1, handle_1) =
        futures::future::abortable(create_swarm(local_key_1, nc_1, addr_1, Peer::Second, nc_out_tx));
    let (cancel_tx_0, cancel_rx_0) = oneshot::channel::<()>();
    let (cancel_tx_1, cancel_rx_1) = oneshot::channel::<()>();

    // Spawn tasks for peer_0
    async_std::task::spawn(async move {
        let _ = cancel_rx_0.await;
        handle_0.abort();
        sync_handler_0_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(6)).await.unwrap();
        cancel_tx_0.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_0);

    // Spawn tasks for peer_1
    async_std::task::spawn(async move {
        let _ = cancel_rx_1.await;
        handle_1.abort();
        sync_handler_1_handle.cancel().await;
    });
    async_std::task::spawn(async move {
        wasm_timer::Delay::new(Duration::from_secs(5)).await.unwrap();
        cancel_tx_1.send(()).unwrap();
    });
    async_std::task::spawn(abortable_peer_1);

    // Collect messages from the peers. Note that the while loop below will end since
    // `abortable_peer_0` and `abortable_peer_1` is guaranteed to drop, leading to the senders
    // dropping too.
    let mut res_peer_0 = vec![];
    let mut res_peer_1 = vec![];
    while let Some((peer, nc_msg)) = nc_out_rx.next().await {
        match peer {
            Peer::First => res_peer_0.push(nc_msg),
            Peer::Second => res_peer_1.push(nc_msg),
        }
    }

    dbg!(&res_peer_0);
    dbg!(&res_peer_1);

    assert_eq!(
        NetworkControllerOut::ConnectedWithInboundPeer(local_peer_id_1),
        res_peer_0[0]
    );
    assert_eq!(
        NetworkControllerOut::PeerPunished {
            peer_id: local_peer_id_1,
            reason: ReputationChange::TooSlow,
        },
        res_peer_0[1]
    );
    assert_eq!(
        Some(&NetworkControllerOut::Disconnected {
            peer_id: local_peer_id_1,
            reason: ConnectionLossReason::Reset(ConnHandlerError::SyncChannelExhausted),
        }),
        res_peer_0.last()
    );
    assert_eq!(
        Some(&NetworkControllerOut::ConnectedWithOutboundPeer(local_peer_id_0)),
        res_peer_1.first()
    );
}

fn make_swarm_components<P, F>(
    peers: Vec<PeerDestination>,
    gen_protocol_behaviour: F,
    msg_buffer_size: usize,
) -> (
    ProtocolHandler<P, NetworkMailbox>,
    NetworkController<PeersMailbox, PeerManager<PeerRepo>, ProtocolMailbox>,
)
where
    P: ProtocolBehaviour + Unpin + std::marker::Send + 'static,
    F: FnOnce(PeersMailbox) -> P,
{
    let peer_conn_handler_conf = PeerConnHandlerConf {
        async_msg_buffer_size: msg_buffer_size,
        sync_msg_buffer_size: msg_buffer_size,
        open_timeout: Duration::from_secs(60),
        initial_keep_alive: Duration::from_secs(60),
    };
    let netw_config = NetworkingConfig {
        min_known_peers: 1,
        min_outbound: 1,
        max_inbound: 10,
        max_outbound: 20,
    };
    let peer_manager_conf = PeerManagerConfig {
        min_acceptable_reputation: Reputation::from(0),
        min_reputation: Reputation::from(0),
        conn_reset_outbound_backoff: Duration::from_secs(120),
        conn_alloc_interval: Duration::from_secs(30),
        prot_alloc_interval: Duration::from_secs(30),
        protocols_allocation: Vec::new(),
    };
    let peer_state = PeerRepo::new(netw_config, peers);
    let (peer_manager, peers) = PeerManager::new(peer_state, peer_manager_conf);
    let sync_conf = ProtocolConfig {
        supported_versions: vec![(
            SyncSpec::v1(),
            ProtocolSpec {
                max_message_size: 100,
                approve_required: true,
            },
        )],
    };

    let (requests_snd, requests_recv) = mpsc::unbounded::<NetworkControllerIn>();
    let network_api = NetworkMailbox {
        mailbox_snd: requests_snd,
    };
    let (sync_handler, sync_mailbox) =
        ProtocolHandler::new(gen_protocol_behaviour(peers.clone()), network_api);
    let nc = NetworkController::new(
        peer_conn_handler_conf,
        HashMap::from([(SYNC_PROTOCOL_ID, (sync_conf, sync_mailbox))]),
        peers,
        peer_manager,
        requests_recv,
    );

    (sync_handler, nc)
}

async fn create_swarm(
    local_key: identity::Keypair,
    nc: NetworkController<PeersMailbox, PeerManager<PeerRepo>, ProtocolMailbox>,
    addr: Multiaddr,
    peer: Peer,
    mut tx: mpsc::Sender<(Peer, NetworkControllerOut)>,
) {
    let transport = libp2p::development_transport(local_key.clone()).await.unwrap();
    let local_peer_id = PeerId::from(local_key.public());
    let mut swarm = Swarm::new(transport, nc, local_peer_id);

    swarm.listen_on(addr).unwrap();

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => println!("Listening on {:?}", address),
            SwarmEvent::Behaviour(event) => tx.try_send((peer, event)).unwrap(),
            ce @ SwarmEvent::ConnectionEstablished { .. } => {
                dbg!(ce);
            }
            _ => {}
        }
    }
}
