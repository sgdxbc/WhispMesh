use std::net::SocketAddr;

use augustus::{
    app,
    boson::{self, QuorumClient, QuorumClock, VerifyQuorumClock},
    event::{
        self,
        erased::{events::Init, session::Sender, Blanket, Buffered, Session, Unify},
        Once, SendEvent,
    },
    lamport_mutex::{self, events::RequestOk, Causal, Lamport, LamportClock, Replicated},
    net::{
        deserialize, dispatch,
        events::Recv,
        session::{tcp, Tcp},
        Detach, Dispatch, IndexNet, InvokeNet,
    },
    pbft,
    worker::{spawning_backend, Submit, Worker},
    workload::{events::InvokeOk, Queue},
};
use rand::thread_rng;
use tokio::{net::TcpListener, sync::mpsc::UnboundedReceiver};
use tokio_util::sync::CancellationToken;

pub enum Event {
    Request,
    Release,
}

pub async fn untrusted_session(
    config: boson_control_messages::Mutex,
    mut events: UnboundedReceiver<Event>,
    upcall: impl SendEvent<RequestOk> + Send + Sync + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use lamport_mutex::Processor;

    let boson_control_messages::Mutex {
        id,
        addrs,
        variant: boson_control_messages::Variant::Untrusted,
        ..
    } = config
    else {
        anyhow::bail!("unimplemented")
    };
    let addr = addrs[id as usize];

    // let tcp_listener = TcpListener::bind(addr).await?;
    let tcp_listener = TcpListener::bind(SocketAddr::from(([0; 4], addr.port()))).await?;
    let mut dispatch_session = event::Session::new();
    let mut processor_session = Session::new();
    let mut causal_net_session = Session::new();

    let mut dispatch = event::Unify(event::Buffered::from(Dispatch::new(
        Tcp::new(addr)?,
        {
            let mut sender = Sender::from(causal_net_session.sender());
            move |buf: &_| lamport_mutex::on_buf(buf, &mut sender)
        },
        Once(dispatch_session.sender()),
    )?));
    let mut processor = Blanket(Unify(Processor::new(
        id,
        addrs.len(),
        |id| (0u32, id),
        Detach(Sender::from(causal_net_session.sender())),
        upcall,
    )));
    let mut causal_net = Blanket(Unify(Causal::new(
        (0, id),
        Box::new(Sender::from(processor_session.sender()))
            as Box<dyn lamport_mutex::SendRecvEvent<LamportClock> + Send + Sync>,
        Box::new(Lamport(Sender::from(causal_net_session.sender()), id))
            as Box<dyn SendEvent<lamport_mutex::Update<LamportClock>> + Send + Sync>,
        lamport_mutex::MessageNet::<_, LamportClock>::new(IndexNet::new(
            dispatch::Net::from(dispatch_session.sender()),
            addrs,
            // intentionally sending loopback messages as expected by processor protocol
            None,
        )),
    )?));
    Sender::from(causal_net_session.sender()).send(Init)?;

    let event_session = {
        let mut sender = Sender::from(processor_session.sender());
        async move {
            while let Some(event) = events.recv().await {
                match event {
                    Event::Request => sender.send(lamport_mutex::events::Request)?,
                    Event::Release => sender.send(lamport_mutex::events::Release)?,
                }
            }
            anyhow::Ok(())
        }
    };
    let tcp_accept_session = tcp::accept_session(tcp_listener, dispatch_session.sender());
    let dispatch_session = dispatch_session.run(&mut dispatch);
    let processor_session = processor_session.run(&mut processor);
    let causal_net_session = causal_net_session.run(&mut causal_net);

    tokio::select! {
        () = cancel.cancelled() => return Ok(()),
        result = event_session => result?,
        result = tcp_accept_session => result?,
        result = dispatch_session => result?,
        result = processor_session => result?,
        result = causal_net_session => result?,
    }
    anyhow::bail!("unreachable")
}

pub async fn replicated_session(
    config: boson_control_messages::Mutex,
    mut events: UnboundedReceiver<Event>,
    upcall: impl SendEvent<RequestOk> + Send + Sync + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use augustus::{
        crypto::{Crypto, CryptoFlavor},
        lamport_mutex::Processor,
    };

    let boson_control_messages::Mutex {
        id,
        addrs,
        variant: boson_control_messages::Variant::Replicated(config),
        ..
    } = config
    else {
        anyhow::bail!("unimplemented")
    };
    let num_faulty = config.num_faulty;
    let addr = addrs[id as usize];
    let num_replica = addrs.len();

    // let client_tcp_listener = TcpListener::bind(SocketAddr::from((addr.ip(), 0))).await?;
    let client_tcp_listener = TcpListener::bind(SocketAddr::from(([0; 4], 0))).await?;
    let client_addr = SocketAddr::from((addr.ip(), client_tcp_listener.local_addr()?.port()));
    // let tcp_listener = TcpListener::bind(addr).await?;
    let tcp_listener = TcpListener::bind(SocketAddr::from(([0; 4], addr.port()))).await?;

    let mut client_dispatch_session = event::Session::new();
    let mut client_session = Session::new();
    let mut dispatch_session = event::Session::new();
    let mut replica_session = Session::new();
    let mut queue_session = Session::new();
    let mut processor_session = Session::new();

    let mut client_dispatch = event::Unify(event::Buffered::from(Dispatch::new(
        Tcp::new(client_addr)?,
        {
            let mut sender = Sender::from(client_session.sender());
            move |buf: &_| pbft::to_client_on_buf(buf, &mut sender)
        },
        Once(client_dispatch_session.sender()),
    )?));
    let mut dispatch = event::Unify(event::Buffered::from(Dispatch::new(
        Tcp::new(addr)?,
        {
            let mut sender = Sender::from(replica_session.sender());
            move |buf: &_| pbft::to_replica_on_buf(buf, &mut sender)
        },
        Once(dispatch_session.sender()),
    )?));
    let mut client = Blanket(Buffered::from(pbft::Client::new(
        id as _,
        client_addr,
        pbft::ToReplicaMessageNet::new(IndexNet::new(
            dispatch::Net::from(client_dispatch_session.sender()),
            addrs.clone(),
            None,
        )),
        Box::new(Sender::from(queue_session.sender()))
            as Box<dyn SendEvent<InvokeOk> + Send + Sync>,
        num_replica,
        num_faulty,
    )));
    let mut replica = Blanket(Buffered::from(pbft::Replica::new(
        id,
        app::OnBuf({
            let mut sender = Replicated::new(Sender::from(processor_session.sender()));
            move |buf: &_| sender.send(Recv(deserialize::<lamport_mutex::Message>(buf)?))
        }),
        pbft::ToReplicaMessageNet::new(IndexNet::new(
            dispatch::Net::from(dispatch_session.sender()),
            addrs,
            id as usize,
        )),
        pbft::ToClientMessageNet::new(dispatch::Net::from(dispatch_session.sender())),
        Box::new(pbft::CryptoWorker::from(Worker::Inline(
            Crypto::new_hardcoded(num_replica, id, CryptoFlavor::Schnorrkel)?,
            Sender::from(replica_session.sender()),
        ))) as Box<dyn Submit<Crypto, dyn pbft::SendCryptoEvent<SocketAddr>> + Send + Sync>,
        num_replica,
        num_faulty,
    )));
    let mut queue = Blanket(Unify(Queue::new(Sender::from(client_session.sender()))));
    let mut processor = Blanket(Unify(Processor::new(
        id,
        num_replica,
        |_| 0u32,
        augustus::net::MessageNet::<_, lamport_mutex::Message>::new(InvokeNet(Sender::from(
            queue_session.sender(),
        ))),
        upcall,
    )));

    let event_session = {
        let mut sender = Sender::from(processor_session.sender());
        async move {
            while let Some(event) = events.recv().await {
                match event {
                    Event::Request => sender.send(lamport_mutex::events::Request)?,
                    Event::Release => sender.send(lamport_mutex::events::Release)?,
                }
            }
            anyhow::Ok(())
        }
    };
    let client_tcp_accept_session =
        tcp::accept_session(client_tcp_listener, client_dispatch_session.sender());
    let tcp_accept_session = tcp::accept_session(tcp_listener, dispatch_session.sender());
    let client_dispatch_session = client_dispatch_session.run(&mut client_dispatch);
    let dispatch_session = dispatch_session.run(&mut dispatch);
    let client_session = client_session.run(&mut client);
    let replica_session = replica_session.run(&mut replica);
    let queue_session = queue_session.run(&mut queue);
    let processor_session = processor_session.run(&mut processor);

    tokio::select! {
        () = cancel.cancelled() => return Ok(()),
        result = event_session => result?,
        result = client_tcp_accept_session => result?,
        result = client_dispatch_session => result?,
        result = tcp_accept_session => result?,
        result = dispatch_session => result?,
        result = client_session => result?,
        result = replica_session => result?,
        result = queue_session => result?,
        result = processor_session => result?,
    }
    anyhow::bail!("unreachable")
}

pub async fn quorum_session(
    config: boson_control_messages::Mutex,
    mut events: UnboundedReceiver<Event>,
    upcall: impl SendEvent<RequestOk> + Send + Sync + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use augustus::{
        crypto::peer::{Crypto, Verifiable},
        lamport_mutex::verifiable::Processor,
    };

    let boson_control_messages::Mutex {
        id,
        addrs,
        variant: boson_control_messages::Variant::Quorum(config),
        num_faulty,
    } = config
    else {
        anyhow::bail!("unimplemented")
    };
    let addr = addrs[id as usize];
    let crypto = Crypto::new_random(&mut thread_rng());

    // let tcp_listener = TcpListener::bind(addr).await?;
    let tcp_listener = TcpListener::bind(SocketAddr::from(([0; 4], addr.port()))).await?;
    // let clock_tcp_listener = TcpListener::bind(SocketAddr::from((addr.ip(), 0))).await?;
    let clock_tcp_listener = TcpListener::bind(SocketAddr::from(([0; 4], 0))).await?;
    let clock_addr = SocketAddr::from((addr.ip(), clock_tcp_listener.local_addr()?.port()));

    let mut dispatch_session = event::Session::new();
    let mut clock_dispatch_session = event::Session::new();
    let mut processor_session = Session::new();
    let mut causal_net_session = Session::new();
    let mut clock_session = Session::new();
    // verify clocked messages sent by other processors before they are received causal net
    // let (recv_crypto_worker, mut recv_crypto_executor) = spawning_backend();
    // owned by quorum client
    let (crypto_worker, mut crypto_executor) = spawning_backend();
    // sign Ordered messages
    let (processor_crypto_worker, mut processor_crypto_executor) = spawning_backend();

    let mut dispatch = event::Unify(event::Buffered::from(Dispatch::new(
        Tcp::new(addr)?,
        {
            // let mut clocked_sender = VerifyQuorumClock::new(config.num_faulty, recv_crypto_worker);
            let mut clocked_sender = VerifyQuorumClock::new(
                config.num_faulty,
                Worker::Inline(crypto.clone(), Sender::from(causal_net_session.sender())),
            );
            let mut sender = Sender::from(processor_session.sender());
            move |buf: &_| lamport_mutex::verifiable::on_buf(buf, &mut clocked_sender, &mut sender)
        },
        Once(dispatch_session.sender()),
    )?));
    let mut clock_dispatch = event::Unify(event::Buffered::from(Dispatch::new(
        Tcp::new(clock_addr)?,
        {
            let mut sender = Sender::from(clock_session.sender());
            move |buf: &_| sender.send(Recv(deserialize::<Verifiable<boson::AnnounceOk>>(buf)?))
        },
        Once(dispatch_session.sender()),
    )?));
    let mut processor = Blanket(Unify(Processor::new(
        id,
        addrs.len(),
        num_faulty,
        |_| QuorumClock::default(),
        Detach(Sender::from(causal_net_session.sender())),
        lamport_mutex::verifiable::SignOrdered::new(processor_crypto_worker),
        upcall,
    )));
    let mut causal_net = Blanket(Unify(Causal::new(
        QuorumClock::default(),
        Box::new(Sender::from(processor_session.sender()))
            as Box<dyn lamport_mutex::SendRecvEvent<QuorumClock> + Send + Sync>,
        boson::Lamport(Sender::from(clock_session.sender()), id as _),
        lamport_mutex::verifiable::MessageNet::<_, QuorumClock>::new(IndexNet::new(
            dispatch::Net::from(dispatch_session.sender()),
            addrs.clone(),
            // intentionally sending loopback messages as expected by processor protocol
            None,
        )),
    )?));
    let mut clock = Blanket(Unify(QuorumClient::new(
        clock_addr,
        crypto.public_key(),
        config.num_faulty,
        Box::new(boson::quorum_client::CryptoWorker::from(crypto_worker))
            as Box<
                dyn Submit<Crypto, dyn boson::quorum_client::SendCryptoEvent<SocketAddr>>
                    + Send
                    + Sync,
            >,
        boson::Lamport(
            Box::new(Sender::from(causal_net_session.sender()))
                as Box<dyn SendEvent<lamport_mutex::events::UpdateOk<QuorumClock>> + Send + Sync>,
            id as _,
        ),
        augustus::net::MessageNet::<_, Verifiable<boson::Announce<SocketAddr>>>::new(
            IndexNet::new(
                dispatch::Net::from(clock_dispatch_session.sender()),
                config.addrs,
                None,
            ),
        ),
    )));

    Sender::from(causal_net_session.sender()).send(Init)?;

    let event_session = {
        let mut sender = Sender::from(processor_session.sender());
        async move {
            while let Some(event) = events.recv().await {
                match event {
                    Event::Request => sender.send(lamport_mutex::events::Request)?,
                    Event::Release => sender.send(lamport_mutex::events::Release)?,
                }
            }
            anyhow::Ok(())
        }
    };
    let tcp_accept_session = tcp::accept_session(tcp_listener, dispatch_session.sender());
    let clock_tcp_accept_session =
        tcp::accept_session(clock_tcp_listener, clock_dispatch_session.sender());
    // let recv_crypto_session =
    //     recv_crypto_executor.run(crypto.clone(), Sender::from(causal_net_session.sender()));
    let crypto_session = crypto_executor.run(crypto.clone(), Sender::from(clock_session.sender()));
    let processor_crypto_session = processor_crypto_executor.run(
        crypto,
        lamport_mutex::verifiable::MessageNet::<_, QuorumClock>::new(IndexNet::new(
            dispatch::Net::from(dispatch_session.sender()),
            addrs,
            None,
        )),
    );
    let dispatch_session = dispatch_session.run(&mut dispatch);
    let clock_dispatch_session = clock_dispatch_session.run(&mut clock_dispatch);
    let processor_session = processor_session.run(&mut processor);
    let causal_net_session = causal_net_session.run(&mut causal_net);
    let clock_session = clock_session.run(&mut clock);

    tokio::select! {
        () = cancel.cancelled() => return Ok(()),
        result = event_session => result?,
        result = tcp_accept_session => result?,
        result = clock_tcp_accept_session => result?,
        result = dispatch_session => result?,
        result = clock_dispatch_session => result?,
        result = processor_session => result?,
        result = causal_net_session => result?,
        // result = recv_crypto_session => result?,
        result = crypto_session => result?,
        result = processor_crypto_session => result?,
        result = clock_session => result?,
    }
    anyhow::bail!("unreachable")
}

// cSpell:words lamport upcall pbft