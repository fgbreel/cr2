use channel::{Channel, ChannelProgress, MAX_PACKET_SIZE};
use clock;
use config;
use dns;
use error::Error;
use headers::Headers;
use identity::{self, Identity};
use local_addrs;
use noise;
use osaka::mio::net::UdpSocket;
use osaka::{osaka, FutureResult};
use packet::{EncryptedPacket, RoutingKey};
use prost::Message;
use proto;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use util::defer;
use rand::seq::SliceRandom;
use rand::thread_rng;
use osaka::Future;
use std::mem;

#[derive(Clone)]
pub struct Stream {
    inner:  Arc<RefCell<Channel>>,
    stream: u32,
    ii:     Arc<Cell<FutureResult<Vec<u8>>>>,
    again:  osaka::Again,
}

impl Stream {
    pub fn send<M: Into<Vec<u8>>>(&mut self, m: M) {
        self.inner
            .try_borrow_mut()
            .expect("carrier is not thread safe")
            .stream(self.stream, m)
    }

    pub fn small_message<M: Message>(&mut self, m: M) {
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();
        self.send(b)
    }

    pub fn message<M: Message>(&mut self, m: M) {
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();

        let mut bh = Vec::new();
        proto::ProtoHeader{len: b.len() as u64}.encode(&mut bh).unwrap();
        self.send(bh);
        for g in b.chunks(600) {
            self.send(g)
        }
    }
}

impl osaka::Future<Vec<u8>> for Stream {
    fn poll(&mut self) -> FutureResult<Vec<u8>> {
        self.ii.replace(FutureResult::Again(self.again.clone()))
    }
}


pub trait StreamFactory {
    fn f(&mut self, Headers, Stream) -> Option<osaka::Task<()>>;
}

impl<F> StreamFactory for F
where
    F: FnMut(Headers, Stream) -> Option<osaka::Task<()>>,
{
    fn f(&mut self, h: Headers, s: Stream) -> Option<osaka::Task<()>> {
        (*self)(h, s)
    }
}

struct StreamReceiver {
    f: osaka::Task<()>,
    a: Arc<Cell<FutureResult<Vec<u8>>>>,
}

enum AddressMode {
    Discovering(HashMap<SocketAddr, (proto::path::Category, usize)>),
    Established(
        SocketAddr,
        HashMap<SocketAddr, (proto::path::Category, usize)>,
    ),
}

struct UdpChannel {
    identity:   Identity,
    chan:       Arc<RefCell<Channel>>,
    addrs:      AddressMode,
    streams:    HashMap<u32, StreamReceiver>,
    newhandl:   Option<Box<StreamFactory>>,
}

impl Drop for UdpChannel {
    fn drop(&mut self) {
        debug!("[{}] udp channel dropped with {} streams", self.identity, self.streams.len());
    }
}

pub struct Endpoint {
    poll:               osaka::Poll,
    token:              osaka::Token,
    channels:           HashMap<RoutingKey, UdpChannel>,
    socket:             UdpSocket,
    broker_route:       RoutingKey,
    secret:             identity::Secret,
    outstanding_connect_incomming: HashSet<u32>,
    outstanding_connect_outgoing:  HashMap<u32, ConnectResponseStage>,
    publish_secret:     Option<identity::Secret>,
}

pub struct ConnectRequest {
    pub qstream: u32,
    pub identity: identity::Identity,
    pub responder: noise::HandshakeResponder,
    pub cr: proto::PeerConnectRequest,
}



enum ConnectResponseStage {
    WaitingForHeaders {
        identity: identity::Identity,
        noise : noise::HandshakeRequester,
    },
    WaitingForResponse {
        identity: identity::Identity,
        noise : noise::HandshakeRequester,
    },
}

pub struct ConnectResponse {
    pub identity:   identity::Identity,
    pub cr:         Option<proto::ConnectResponse>,
    pub requester:  Option<noise::HandshakeRequester>,
}

impl Endpoint {
    pub fn new(
        poll: osaka::Poll,
        token: osaka::Token,
        noise: noise::Transport,
        identity: identity::Identity,
        socket: UdpSocket,
        addr: SocketAddr,
        secret: identity::Secret,
    ) -> Self {
        let broker_route = noise.route();
        let mut channels = HashMap::new();
        let debug_id = format!("{}::{}", broker_route, identity);
        channels.insert(
            noise.route(),
            UdpChannel {
                identity,
                chan:       Arc::new(RefCell::new(Channel::new(noise, debug_id))),
                addrs:      AddressMode::Established(addr, HashMap::new()),
                streams:    HashMap::new(),
                newhandl:   None,
            },
        );

        Self {
            poll,
            token,
            channels,
            socket,
            broker_route,
            secret,
            outstanding_connect_incomming: HashSet::new(),
            outstanding_connect_outgoing: HashMap::new(),
            publish_secret: None,
        }
    }



    pub fn broker(&self) -> RoutingKey {
        self.broker_route
    }

    #[osaka]
    fn publish_stream(poll: osaka::Poll, mut stream: Stream) {
        let _omg = defer(|| {
            panic!("publish closed");
        });



        let m = osaka::sync!(stream);
        let headers = Headers::decode(&m).unwrap();
        info!("pubres: {:?}", headers);

        yield poll.never();
    }

    pub fn publish(&mut self, shadow: identity::Address) {
        if self.publish_secret.is_none() {
            self.publish_secret = Some(identity::Secret::gen());
        }
        let xaddr = identity::SignedAddress::sign(
            &self.secret,
            self.publish_secret.as_ref().unwrap().address(),
        );

        let broker = self.broker_route;
        self.open(
            broker,
            Headers::with_path("/carrier.broker.v1/broker/publish"),
            |poll, mut stream| {
                stream.small_message(proto::PublishRequest{
                    xaddr: xaddr.to_vec(),
                    shadow: shadow.as_bytes().to_vec(),
                });
                Self::publish_stream(poll, stream)
            },
        );
    }

    pub fn connect(&mut self, target: identity::Identity) -> Result<(), Error> {

        let timestamp = clock::network_time();
        let (noise, pkt) = noise::initiate(None, &self.secret, timestamp)?;
        let handshake = pkt.encode();

        let mut mypaths = Vec::new();
        for addr in local_addrs::get(self.socket.local_addr().unwrap().port()) {
            mypaths.push(proto::Path {
                category: (proto::path::Category::Local as i32),
                ipaddr: format!("{}", addr),
            });
        }

        let chan = self.channels.get_mut(&self.broker_route).unwrap();
        let stream_id = {
            let mut chanchan = chan
                .chan
                .try_borrow_mut()
                .expect("carrier is not thread safe");
            let stream_id = chanchan.open(Headers::with_path("/carrier.broker.v1/broker/connect").encode(), true);

            let mut m = Vec::new();
            proto::ConnectRequest{
                identity: target.as_bytes().to_vec(),
                timestamp,
                handshake,
                paths: mypaths,
            }.encode(&mut m).unwrap();
            chanchan.stream(stream_id, m);

            stream_id
        };

        self.outstanding_connect_outgoing.insert(stream_id, ConnectResponseStage::WaitingForHeaders{
            identity: target,
            noise,
        });

        Ok(())
    }

    pub fn reject(&mut self, q: ConnectRequest) {
        let mut m = Vec::new();
        proto::PeerConnectResponse {
            ok:         false,
            handshake:  Vec::new(),
            paths:      Vec::new(),
        }
        .encode(&mut m)
        .unwrap();
        let broker_route = self.broker_route;
        self.stream(broker_route, q.qstream, m);
    }

    pub fn accept_outgoing<F: 'static + StreamFactory>(&mut self, q: ConnectResponse, sf: F) -> Result<RoutingKey, Error>{
        let identity = q.identity;
        let (cr, mut requester) = match (q.cr, q.requester) {
            (Some(a), Some(b)) => (a,b),
            (cr,_) => return Err(Error::OutgoingConnectFailed{identity: identity, cr}),
        };

        if cr.ok != true {
            return Err(Error::OutgoingConnectFailed{identity, cr:Some(cr)});
        }

        let pkt         = EncryptedPacket::decode(&cr.handshake)?;
        let hs_identity = requester.recv_response(pkt).unwrap();
        let noise       = requester.into_transport()?;

        if identity != hs_identity {
            panic!("SECURITY ALERT: handshake for outgoing connect has unexpected identity");
        }
        if cr.route != noise.route() {
            panic!("BUG (in broker maybe): handshake for outgoing connect has unexpected route");
        }


        let mut paths = HashMap::new();
        for path in cr.paths {
            let cat = match path.category {
                o if proto::path::Category::Local as i32 == o => proto::path::Category::Local,
                o if proto::path::Category::Internet as i32 == o => proto::path::Category::Internet,
                o if proto::path::Category::BrokerOrigin as i32 == o => {
                    proto::path::Category::BrokerOrigin
                }
                _ => unreachable!(),
            };
            paths.insert(path.ipaddr.parse().unwrap(), (cat, 0));
        }
        if let Some(chan) = self.channels.get(&self.broker_route) {
            if let AddressMode::Established(addr, _) = chan.addrs {
                paths.insert(addr.clone(), (proto::path::Category::BrokerOrigin, 0));
            }
        }

        let debug_id = format!("{}::{}", identity, cr.route);
        self.channels.insert(
            cr.route,
            UdpChannel {
                identity,
                chan: Arc::new(RefCell::new(Channel::new(noise, debug_id))),
                addrs: AddressMode::Discovering(paths.clone()),
                streams: HashMap::new(),
                newhandl: Some(Box::new(sf)),
            },
        );

        Ok(cr.route)
    }

    pub fn accept_incomming<F: 'static + StreamFactory>(&mut self, q: ConnectRequest, sf: F) {
        let (noise, pkt) = q
            .responder
            .send_response(q.cr.route, &self.secret)
            .expect("send_response");

        let mut paths = HashMap::new();
        for path in q.cr.paths {
            let cat = match path.category {
                o if proto::path::Category::Local as i32 == o => proto::path::Category::Local,
                o if proto::path::Category::Internet as i32 == o => proto::path::Category::Internet,
                o if proto::path::Category::BrokerOrigin as i32 == o => {
                    proto::path::Category::BrokerOrigin
                }
                _ => unreachable!(),
            };
            paths.insert(path.ipaddr.parse().unwrap(), (cat, 0));
        }
        if let Some(chan) = self.channels.get(&self.broker_route) {
            if let AddressMode::Established(addr, _) = chan.addrs {
                paths.insert(addr.clone(), (proto::path::Category::BrokerOrigin, 0));
            }
        }

        let debug_id = format!("{}::{}", q.identity, q.cr.route);
        self.channels.insert(
            q.cr.route,
            UdpChannel {
                identity: q.identity,
                chan: Arc::new(RefCell::new(Channel::new(noise, debug_id))),
                addrs: AddressMode::Discovering(paths.clone()),
                streams: HashMap::new(),
                newhandl: Some(Box::new(sf)),
            },
        );

        let mut mypaths = Vec::new();
        for addr in local_addrs::get(self.socket.local_addr().unwrap().port()) {
            mypaths.push(proto::Path {
                category: (proto::path::Category::Local as i32),
                ipaddr: format!("{}", addr),
            });
        }

        let mut m = Vec::new();
        proto::PeerConnectResponse {
            ok: true,
            handshake: pkt.encode(),
            paths: mypaths,
        }
        .encode(&mut m)
        .unwrap();

        let broker_route = self.broker_route;
        self.stream(broker_route, q.qstream, m);
    }

    pub fn open<F>(&mut self, route: RoutingKey, headers: Headers, f: F)
    where
        F: FnOnce(osaka::Poll, Stream) -> osaka::Task<()>,
    {
        let chan = self.channels.get_mut(&route).unwrap();

        let stream_id = {
            let mut chanchan = chan
                .chan
                .try_borrow_mut()
                .expect("carrier is not thread safe");
            let stream_id = chanchan.open(headers.encode(), true);
            stream_id
        };

        let again = self.poll.never();
        let ii = Arc::new(Cell::new(FutureResult::Again(again.clone())));
        let stream = Stream {
            inner:  chan.chan.clone(),
            stream: stream_id,
            ii:     ii.clone(),
            again,
        };
        chan.streams.insert(
            stream_id,
            StreamReceiver {
                f: f(self.poll.clone(), stream),
                a: ii,
            },
        );
    }

    pub fn stream<M: Into<Vec<u8>>>(&mut self, route: RoutingKey, stream: u32, m: M) {
        let chan = self.channels.get_mut(&route).unwrap();
        let mut chanchan = chan
            .chan
            .try_borrow_mut()
            .expect("carrier is not thread safe");
        chanchan.stream(stream, m)
    }

    fn peer_connect_request(
        qstream: u32,
        publish_secret: &identity::Secret,
        frame: Vec<u8>,
    ) -> Result<ConnectRequest, Error> {
        let cr = proto::PeerConnectRequest::decode(&frame)?;
        let identity = identity::Identity::from_bytes(&cr.identity)?;
        let pkt = EncryptedPacket::decode(&cr.handshake)?;
        let (responder, id2, ts) = noise::respond(None, pkt)?;

        if id2 != identity || ts != cr.timestamp {
            return Err(Error::SecurityViolation);
        }

        Ok(ConnectRequest {
            identity,
            responder,
            cr,
            qstream,
        })
    }
}


pub enum Event {
    IncommingConnect(ConnectRequest),
    OutgoingConnect(ConnectResponse),
    Disconnect{
        route: RoutingKey,
        identity: Identity
    },
}



impl Future<Result<Event, Error>> for Endpoint {
    fn poll(&mut self) -> FutureResult<Result<Event, Error>> {
        // receive one packet
        let mut buf = vec![0; MAX_PACKET_SIZE];
        match self.socket.recv_from(&mut buf) {
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    return FutureResult::Done(Err(Error::Io(e)));
                }
            }
            Ok((len, addr)) => match EncryptedPacket::decode(&buf[..len]) {
                Err(e) => warn!("{}: {}", addr, e),
                Ok(pkt) => {
                    if let Some(chan) = self.channels.get_mut(&pkt.route) {

                        let settle = if let AddressMode::Discovering(ref mut addrs) = chan.addrs {
                            trace!("in discovery: received from {}", addr);
                            let count = {
                                let (_, count) = addrs.entry(addr).or_insert((proto::path::Category::Internet, 0));
                                *count += 1;
                                *count
                            };
                            if count >= 5 {
                                let mut m = None;
                                let mut bestest = None;
                                for (addr, (cat, count)) in &*addrs {
                                    if *count >= 1 {
                                        if let Some(ref mut bestest) = bestest {
                                            if *bestest > *cat as i32 {
                                                m = Some(addr.clone());
                                                *bestest = *cat as i32;
                                            }
                                        } else {
                                            m = Some(addr.clone());
                                            bestest = Some(*cat as i32);
                                        }
                                    }
                                }
                                Some((m.unwrap(), mem::replace(addrs, HashMap::new())))
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        if let Some((addr, previous)) = settle {
                            info!("settled peering with adress {}", addr);
                            chan.addrs = AddressMode::Established(addr, previous);
                        }

                        let mut chanchan = chan
                            .chan
                            .try_borrow_mut()
                            .expect("carrier is not thread safe");
                        match chanchan.recv(pkt) {
                            Err(Error::AntiReplay) => debug!("{}: {}", addr, Error::AntiReplay),
                            Err(e) => warn!("{}: {}", addr, e),
                            Ok(()) => {
                                if let AddressMode::Established(ref mut addr_, ref previous) = chan.addrs {
                                    if addr != *addr_ {
                                        let current_cat = previous.get(addr_).unwrap_or(&(proto::path::Category::Internet, 0)).0;
                                        let migrate_cat = previous.get(&addr).unwrap_or(&(proto::path::Category::Internet, 0)).0;

                                        if current_cat as i32 >= migrate_cat as i32 {
                                            warn!(
                                                "channel migration not fully implemented yet. migrating from  {} to {}",
                                                addr_, addr,
                                                );
                                            *addr_ = addr;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        };

        // work on all channels
        let mut later = self
            .poll
            .again(self.token.clone(), Some(Duration::from_secs(600)));
        loop {
            let mut again = false;
            let mut killme = Vec::new();
            for (route, chan) in &mut self.channels {
                //TODO: DRY this up. we need this so that packets queued by drivers are sent out immediately
                // shake every stream again
                let keys: Vec<u32> = chan.streams.iter().map(|(k, _)| *k).collect();
                for stream in keys {
                    let mut closed = false;
                    if let Some(driver) = chan.streams.get_mut(&stream) {
                        match driver.f.poll() {
                            osaka::FutureResult::Done(()) => {
                                closed = true;
                            }
                            osaka::FutureResult::Again(a2) => {
                                later.merge(a2);
                            }
                        }
                    }
                    if closed {
                        debug!("stream {} was closed by this end", stream);
                        chan.streams.remove(&stream);
                        let mut chanchan = chan
                            .chan
                            .try_borrow_mut()
                            .expect("carrier is not thread safe");
                        chanchan.close(stream);
                    }
                }

                let r = {
                    let mut chanchan = chan
                        .chan
                        .try_borrow_mut()
                        .expect("carrier is not thread safe");

                    osaka::try!(chanchan.progress())
                };
                match r {
                    ChannelProgress::Later(dur) => {
                        later.merge(self.poll.later(dur));
                    }
                    ChannelProgress::SendPacket(pkt) => {
                        again = true;
                        match &chan.addrs {
                            AddressMode::Discovering(addrs) => {
                                for (addr, _) in addrs.iter() {
                                    match self.socket.send_to(&pkt, addr) {
                                        Ok(len) if len == pkt.len() => (),
                                        e => trace!("send to {} didnt work {:?}", addr, e),
                                    }
                                }
                            }
                            AddressMode::Established(addr, _) => {
                                match self.socket.send_to(&pkt, &addr) {
                                    Ok(len) if len == pkt.len() => (),
                                    e => error!("send didnt work {:?}", e),
                                }
                            }
                        }
                    }
                    ChannelProgress::ReceiveHeader(stream, frame) => {
                        let headers = osaka::try!(Headers::decode(&frame));
                        debug!("incomming request {:?}", headers);

                        if route == &self.broker_route {
                            let m = match headers.path().as_ref() {
                                Some(&b"/carrier.broker.v1/peer/connect") => {
                                    self.outstanding_connect_incomming.insert(stream);
                                    Headers::ok()
                                }
                                _ => Headers::with_error(404, "not found"),
                            };
                            let mut chanchan = chan
                                .chan
                                .try_borrow_mut()
                                .expect("carrier is not thread safe");
                            chanchan.stream(stream, m.encode());
                        } else {
                            if let Some(ref mut new) = chan.newhandl {
                                let again = self.poll.never();
                                let ii = Arc::new(Cell::new(FutureResult::Again(again.clone())));
                                let mut stream = Stream {
                                    inner: chan.chan.clone(),
                                    stream,
                                    ii: ii.clone(),
                                    again,
                                };

                                if let Some(f) = new.f(headers, stream.clone()) {
                                    chan.streams
                                        .insert(stream.stream, StreamReceiver { f, a: ii.clone() });
                                } else {
                                    let mut chanchan = chan
                                        .chan
                                        .try_borrow_mut()
                                        .expect("carrier is not thread safe");
                                    chanchan.close(stream.stream);
                                }
                            }
                        }

                        again = true;
                    }
                    ChannelProgress::ReceiveStream(stream, frame) => {
                        if route == &self.broker_route
                            && self.outstanding_connect_incomming.remove(&stream)
                            && self.publish_secret.is_some()
                        {
                            match Self::peer_connect_request(
                                stream,
                                self.publish_secret.as_ref().unwrap(),
                                frame,
                            ) {
                                Ok(q) => return FutureResult::Done(Ok(Event::IncommingConnect(q))),
                                Err(e) => {
                                    warn!("{}", e);
                                    let mut m = Vec::new();
                                    proto::PeerConnectResponse {
                                        ok: false,
                                        handshake: Vec::new(),
                                        paths: Vec::new(),
                                    }
                                    .encode(&mut m)
                                    .unwrap();
                                    let mut chanchan = chan
                                        .chan
                                        .try_borrow_mut()
                                        .expect("carrier is not thread safe");
                                    chanchan.stream(stream, m);
                                    chanchan.close(stream);
                                }
                            }
                        } else if route == &self.broker_route &&
                            self.outstanding_connect_outgoing.contains_key(&stream)
                        {
                            let mut cr = self.outstanding_connect_outgoing.remove(&stream).unwrap();
                            match cr {
                                ConnectResponseStage::WaitingForHeaders{identity, noise} => {
                                    let headers = Headers::decode(&frame).unwrap();
                                    trace!("conres: {:?}", headers);
                                    self.outstanding_connect_outgoing.insert(
                                        stream, ConnectResponseStage::WaitingForResponse{
                                            identity, noise});
                                },
                                ConnectResponseStage::WaitingForResponse{identity, noise} => {
                                    let cr = proto::ConnectResponse::decode(&frame).unwrap();
                                    trace!("conres: {:?}", cr);
                                    chan
                                        .chan
                                        .try_borrow_mut()
                                        .expect("carrier is not thread safe")
                                        .close(stream);

                                    return FutureResult::Done(Ok(Event::OutgoingConnect(ConnectResponse{
                                        identity,
                                        requester: Some(noise),
                                        cr: Some(cr),
                                    })));

                                },
                            }

                        } else if let Some(driver) = chan.streams.get_mut(&stream) {
                            driver.a.set(osaka::FutureResult::Done(frame));
                            driver.f.wakeup_now();
                        } else {
                            warn!("[{}] received frame {:?} for unregistered stream {}",
                                  chan.chan
                                  .try_borrow()
                                  .map(|v|v.debug_id.clone())
                                  .unwrap_or(String::from("?")),
                                  frame, stream);
                        }

                        again = true;
                    }
                    ChannelProgress::Close(stream) => {
                        chan.streams.remove(&stream);
                        again = true;
                        if route == &self.broker_route &&
                        self.outstanding_connect_outgoing.contains_key(&stream)
                        {
                            return FutureResult::Done(Ok(Event::OutgoingConnect(ConnectResponse{
                                identity: match self.outstanding_connect_outgoing.remove(&stream).unwrap() {
                                    ConnectResponseStage::WaitingForHeaders{identity, ..}  => identity,
                                    ConnectResponseStage::WaitingForResponse{identity, ..} => identity,
                                },
                                cr: None,
                                requester: None,
                            })));
                        }
                    }
                    ChannelProgress::Disconnect => {
                        debug!("disconnect {}", route);
                        killme.push(route.clone());
                    }
                };

                // poke every stream again
                let keys: Vec<u32> = chan.streams.iter().map(|(k, _)| *k).collect();
                for stream in keys {
                    let mut closed = false;
                    if let Some(driver) = chan.streams.get_mut(&stream) {
                        match driver.f.poll() {
                            osaka::FutureResult::Done(()) => {
                                closed = true;
                            }
                            osaka::FutureResult::Again(a2) => {
                                later.merge(a2);
                            }
                        }
                    }
                    if closed {
                        debug!("stream {} was closed by this end", stream);
                        chan.streams.remove(&stream);
                        let mut chanchan = chan
                            .chan
                            .try_borrow_mut()
                            .expect("carrier is not thread safe");
                        chanchan.close(stream);
                    }
                }
            }

            for killme in killme {
                let rm = self.channels.remove(&killme);
                debug!(
                    "removed channel {}. now managing {} channels",
                    killme,
                    self.channels.len()
                );

                if let Some(rm) = rm {
                    return FutureResult::Done(Ok(Event::Disconnect{
                        route: killme,
                        identity: rm.identity.clone(),
                    }));
                }
            }
            if !again {
                break;
            }
        }

        FutureResult::Again(later)
    }
}

// -- builder

pub struct EndpointBuilder {
    secret: identity::Secret,
}

impl EndpointBuilder {
    pub fn new(config: &config::Config) -> Result<Self, Error> {
        info!("my identity: {}", config.secret.identity());

        Ok(Self {
            secret: config.secret.clone(),
        })
    }

    #[osaka]
    pub fn connect(
        self,
        poll: osaka::Poll,
    ) -> Result<Endpoint, Error> {

        let mut a = osaka_dns::resolve(
            poll.clone(),
            vec![
            "x.carrier.devguard.io".into(),
            "3.carrier.devguard.io".into(),
            ],
            );
        let mut records: Vec<dns::DnsRecord> = osaka::sync!(a)?
            .into_iter()
            .filter_map(|v| dns::DnsRecord::from_signed_txt(v))
            .collect();
        records.shuffle(&mut thread_rng());

        loop {
            let record = match records.pop() {
                Some(v) => v,
                None => return Err(Error::OutOfOptions),
            };

            info!("attempting connection with {}", &record.addr);

            let timestamp = clock::dns_time(&record);
            let (mut noise, pkt) = noise::initiate(Some(&record.x), &self.secret, timestamp)?;
            let pkt = pkt.encode();

            let sock = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap()).map_err(|e| Error::Io(e))?;
            let token = poll
                .register(&sock, mio::Ready::readable(), mio::PollOpt::level())
                .unwrap();

            let mut attempts = 0;
            let r = loop {
                attempts += 1;
                if attempts > 4 {
                    break None;
                }
                let mut buf = vec![0; MAX_PACKET_SIZE];
                if let Ok((len, _from)) = sock.recv_from(&mut buf) {
                    match EncryptedPacket::decode(&buf[..len])
                        .and_then(|pkt| noise.recv_response(pkt))
                    {
                        Ok(identity) => {
                            let noise = noise.into_transport()?;
                            break Some((identity, noise));
                        }
                        Err(e) => {
                            warn!("EndpointFuture::WaitingForResponse: {}", e);
                            continue;
                        }
                    }
                };
                sock.send_to(&pkt, &record.addr)?;
                yield poll.again(
                    token.clone(),
                    Some(Duration::from_millis(2u64.pow(attempts) * 200)),
                );
            };
            let (identity, noise) = match r {
                Some(v) => v,
                None => continue,
            };

            info!(
                "established connection with {} :: {}",
                identity,
                noise.route()
            );

            return Ok(Endpoint::new(
                poll,
                token,
                noise,
                identity,
                sock,
                record.addr,
                self.secret,
            ));
        }
    }
}
