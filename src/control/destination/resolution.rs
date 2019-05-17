use indexmap::{IndexMap, IndexSet};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::SocketAddr,
};

use futures::{task, Async, Poll, Stream};
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::{
    destination::{
        client::Destination, protocol_hint::Protocol, update::Update as PbUpdate2, GetDestination,
        TlsIdentity, Update as PbUpdate, WeightedAddr,
    },
    net::TcpAddress,
};

use control::{
    destination::{Metadata, ProtocolHint, Update},
    remote_stream::{self, Remote},
};

use identity;
use never::Never;
use proxy::resolve;
use NameAddr;

use super::Client;

/// Holds the state of a single resolution.
pub struct Resolution<T>
where
    T: GrpcService<BoxBody>,
{
    auth: NameAddr,
    cache: Cache,
    inner: Option<Inner<T>>,
}

struct Inner<T>
where
    T: GrpcService<BoxBody>,
{
    client: Client<T>,
    query: Query<T>,
}

type Query<T> = remote_stream::Receiver<PbUpdate, T>;

#[derive(Debug, Default)]
struct Cache {
    /// Used to "flatten" destination service responses containing multiple
    /// endpoints into a series of `destination::Update`s for single endpoints.
    queue: VecDeque<Update<Metadata>>,
    /// Tracks all the endpoint addresses we've seen, so that we can send
    /// `Update::Remove`s for them if we recieve a `NoEndpoints` response.
    addrs: IndexSet<SocketAddr>,
}

struct DisplayUpdate<'a>(&'a Update<Metadata>);

impl<T> resolve::Resolution for Resolution<T>
where
    T: GrpcService<BoxBody>,
{
    type Endpoint = Metadata;
    type Error = Never;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        loop {
            trace!("poll resolution");
            if let Some(update) = self.cache.next_update() {
                trace!("{} for {}", DisplayUpdate(&update), self.auth);
                return Ok(Async::Ready(update));
            }

            let canceled = if let Some(inner) = self.inner.as_mut() {
                match inner.poll_update(&self.auth, &mut self.cache) {
                    Ok(Async::Ready(())) => false,
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(ref status) if status.code() == grpc::Code::InvalidArgument => {
                        // Invalid Argument is returned to indicate that the
                        // requested name should *not* query the destination
                        // service. In this case, do not attempt to reconnect.
                        debug!(
                            "Destination.Get stream ended for {} with Invalid Argument",
                            self.auth
                        );
                        self.cache.no_endpoints();
                        true
                    }
                    Err(err) => {
                        warn!("Destination.Get stream errored for {}: {}", self.auth, err,);
                        inner.reconnect(&self.auth);
                        false
                    }
                }
            } else {
                self.cache.no_endpoints();
                false
            };

            if canceled {
                self.inner.take();
            }
        }
    }
}

impl<T> Resolution<T>
where
    T: GrpcService<BoxBody>,
{
    pub(super) fn new(auth: NameAddr, mut client: Client<T>) -> Self {
        let query = client.query(&auth, "connect");
        Self {
            auth,
            inner: Some(Inner { query, client }),
            cache: Cache::default(),
        }
    }

    pub(super) fn none(auth: NameAddr) -> Self {
        let mut cache = Cache::default();
        cache.no_endpoints();
        Self {
            auth,
            cache,
            inner: None,
        }
    }
}

// ===== impl Inner =====
impl<T> Inner<T>
where
    T: GrpcService<BoxBody>,
{
    fn poll_update(&mut self, auth: &NameAddr, cache: &mut Cache) -> Poll<(), grpc::Status> {
        match try_ready!(self.query.poll()) {
            Some(update) => match update.update {
                Some(PbUpdate2::Add(a_set)) => {
                    let set_labels = a_set.metric_labels;
                    let addrs = a_set
                        .addrs
                        .into_iter()
                        .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                    cache.add(addrs);
                }
                Some(PbUpdate2::Remove(r_set)) => {
                    let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                    cache.remove(addrs);
                }
                Some(PbUpdate2::NoEndpoints(_)) => cache.no_endpoints(),
                None => (),
            },
            None => {
                trace!("Destination.Get stream ended for {}, reconnecting", auth);
                self.reconnect(auth);
            }
        };

        Ok(Async::Ready(()))
    }

    fn reconnect(&mut self, auth: &NameAddr) {
        self.query = self.client.query(auth, "reconnect");
    }
}

// ===== impl Cache =====

impl Cache {
    fn next_update(&mut self) -> Option<Update<Metadata>> {
        self.queue.pop_front()
    }

    fn add(&mut self, addrs: impl Iterator<Item = (SocketAddr, Metadata)>) {
        for (addr, meta) in addrs {
            self.queue.push_back(Update::Add(addr, meta));
            self.addrs.insert(addr);
        }
    }

    fn remove(&mut self, addrs: impl Iterator<Item = SocketAddr>) {
        for addr in addrs {
            self.queue.push_back(Update::Remove(addr));
            self.addrs.remove(&addr);
        }
    }

    fn no_endpoints(&mut self) {
        self.queue.clear();
        self.queue.push_front(Update::NoEndpoints);
        for addr in self.addrs.drain(..) {
            self.queue.push_back(Update::Remove(addr));
        }
    }
}

// ===== impl Client =====

impl<T> Client<T>
where
    T: GrpcService<BoxBody>,
{
    /// Attepts to initiate a query to the Destination service if the given
    /// authority matches the client's set of search suffixes.
    ///
    /// # Returns
    /// - `None` if the authority is not suitable for querying the Destination
    //     service, or the underlying client service is `None`,
    /// - `Some(Query)` if the authority is suitable for querying the
    ///    Destination service.
    fn query(&mut self, dst: &NameAddr, connect_or_reconnect: &str) -> Query<T> {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, dst);
        // if self
        //     .client
        //     .poll_ready()
        //     .map(|a| !a.is_ready())
        //     .unwrap_or(false)
        // {
        //     trace!("-> destination client not ready, will need to retry");
        //     return Remote::NeedsReconnect;
        // }
        // trace!("-> destination client is ready");
        let req = GetDestination {
            scheme: "k8s".into(),
            path: format!("{}", dst),
            context_token: self.context_token.as_ref().clone(),
        };
        let mut svc = Destination::new(self.client.as_service());
        let response = svc.get(grpc::Request::new(req));
        remote_stream::Receiver::new(response)
    }
}

impl<'a> fmt::Display for DisplayUpdate<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Update::Remove(ref addr) => write!(f, "remove {}", addr),
            Update::Add(ref addr, ..) => write!(f, "insert {}", addr),
            Update::NoEndpoints => "no endpoints".fmt(f),
        }
    }
}

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
fn pb_to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let addr = pb.addr.and_then(pb_to_sock_addr)?;

    let meta = {
        let mut t = set_labels
            .iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
    };

    let mut proto_hint = ProtocolHint::Unknown;
    if let Some(hint) = pb.protocol_hint {
        if let Some(proto) = hint.protocol {
            match proto {
                Protocol::H2(..) => {
                    proto_hint = ProtocolHint::Http2;
                }
            }
        }
    }

    let tls_id = pb.tls_identity.and_then(pb_to_id);
    let meta = Metadata::new(meta, proto_hint, tls_id, pb.weight);
    Some((addr, meta))
}

fn pb_to_id(pb: TlsIdentity) -> Option<identity::Name> {
    use api::destination::tls_identity::Strategy;

    let Strategy::DnsLikeIdentity(i) = pb.strategy?;
    match identity::Name::from_hostname(i.name.as_bytes()) {
        Ok(i) => Some(i),
        Err(_) => {
            warn!("Ignoring invalid identity: {}", i.name);
            None
        }
    }
}

fn pb_to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use api::net::ip_address::Ip;
    use std::net::{Ipv4Addr, Ipv6Addr};
    /*
    current structure is:
    TcpAddress {
        ip: Option<IpAddress {
            ip: Option<enum Ip {
                Ipv4(u32),
                Ipv6(IPv6 {
                    first: u64,
                    last: u64,
                }),
            }>,
        }>,
        port: u32,
    }
    */
    match pb.ip {
        Some(ip) => match ip.ip {
            Some(Ip::Ipv4(octets)) => {
                let ipv4 = Ipv4Addr::from(octets);
                Some(SocketAddr::from((ipv4, pb.port as u16)))
            }
            Some(Ip::Ipv6(v6)) => {
                let octets = [
                    (v6.first >> 56) as u8,
                    (v6.first >> 48) as u8,
                    (v6.first >> 40) as u8,
                    (v6.first >> 32) as u8,
                    (v6.first >> 24) as u8,
                    (v6.first >> 16) as u8,
                    (v6.first >> 8) as u8,
                    v6.first as u8,
                    (v6.last >> 56) as u8,
                    (v6.last >> 48) as u8,
                    (v6.last >> 40) as u8,
                    (v6.last >> 32) as u8,
                    (v6.last >> 24) as u8,
                    (v6.last >> 16) as u8,
                    (v6.last >> 8) as u8,
                    v6.last as u8,
                ];
                let ipv6 = Ipv6Addr::from(octets);
                Some(SocketAddr::from((ipv6, pb.port as u16)))
            }
            None => None,
        },
        None => None,
    }
}
