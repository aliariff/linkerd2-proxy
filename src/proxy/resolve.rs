extern crate linkerd2_router as rt;
extern crate tower_discover;

use futures::{stream::FuturesUnordered, sync::oneshot, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use std::{
    fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub use self::tower_discover::Change;
use proxy::Error;
use svc;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;

    fn resolve(&self, target: &T) -> Self::Resolution;
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}

pub trait HasEndpointStatus {
    fn endpoint_status(&self) -> EndpointStatus;
}

#[derive(Clone, Debug)]
pub struct EndpointStatus(Arc<AtomicBool>);

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
    NoEndpoints,
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<R, M> {
    resolve: R,
    inner: M,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Discover<R: Resolution, M: svc::Service<R::Endpoint>> {
    resolution: R,
    make: M,
    makes: MakeStream<M::Future>,
    is_empty: Arc<AtomicBool>,
}

struct MakeStream<F> {
    futures: FuturesUnordered<MakeFuture<F>>,
    cancellations: IndexMap<SocketAddr, oneshot::Sender<()>>,
}

struct MakeFuture<F> {
    inner: F,
    canceled: oneshot::Receiver<()>,
    addr: SocketAddr,
}

enum MakeError<E> {
    Inner(E),
    Canceled,
}

// === impl Layer ===

pub fn layer<T, R>(resolve: R) -> Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
{
    Layer { resolve }
}

impl<R, M> svc::Layer<M> for Layer<R>
where
    R: Clone,
{
    type Service = MakeSvc<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            resolve: self.resolve.clone(),
            inner,
        }
    }
}

// === impl MakeSvc ===

impl<T, R, M> svc::Service<T> for MakeSvc<R, M>
where
    R: Resolve<T>,
    R::Endpoint: fmt::Debug,
    M: svc::Service<R::Endpoint> + Clone,
{
    type Response = Discover<R::Resolution, M>;
    type Error = never::Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Discover
    }

    fn call(&mut self, target: T) -> Self::Future {
        let resolution = self.resolve.resolve(&target);
        futures::future::ok(Discover {
            resolution,
            make: self.inner.clone(),
            makes: MakeStream {
                futures: FuturesUnordered::new(),
                cancellations: IndexMap::new(),
            },
            is_empty: Arc::new(AtomicBool::new(false)),
        })
    }
}

// === impl Discover ===

impl<R, M> HasEndpointStatus for Discover<R, M>
where
    R: Resolution,
    M: svc::Service<R::Endpoint>,
{
    fn endpoint_status(&self) -> EndpointStatus {
        EndpointStatus(self.is_empty.clone())
    }
}

impl<R, M> tower_discover::Discover for Discover<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    R::Error: Into<Error>,
    M: svc::Service<R::Endpoint>,
    M::Error: Into<Error>,
{
    type Key = SocketAddr;
    type Service = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        loop {
            if let Async::Ready(Some((addr, svc))) = self.makes.poll().map_err(Into::into)? {
                self.is_empty.store(false, Ordering::Release);
                return Ok(Async::Ready(Change::Insert(addr, svc)));
            }

            try_ready!(self.make.poll_ready().map_err(Into::into));

            let up = try_ready!(self.resolution.poll().map_err(Into::into));
            trace!("watch: {:?}", up);
            match up {
                Update::Add(addr, target) => {
                    // Start building the service and continue.
                    let fut = self.make.call(target);
                    self.makes.push(addr, fut);
                }
                Update::Remove(addr) => {
                    // If the service is still pending, cancel it. It won't
                    // actually be removed until a subsequent poll, however.
                    self.makes.remove(&addr);
                    return Ok(Async::Ready(Change::Remove(addr)));
                }
                Update::NoEndpoints => {
                    // Mark the service as explicitly empty. It's expected that
                    // Remove events have already or will be receieved to
                    // actually empty the receiver.
                    self.is_empty.store(true, Ordering::Release);
                }
            }
        }
    }
}

// === impl EndpointStatus ===

impl EndpointStatus {
    pub fn is_empty(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

// === impl MakeStream ===

impl<F> MakeStream<F> {
    fn push(&mut self, addr: SocketAddr, inner: F) {
        let (cancel, canceled) = oneshot::channel();
        self.cancellations.insert(addr, cancel);
        self.futures.push(MakeFuture {
            addr,
            inner,
            canceled,
        });
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if let Some(cancel) = self.cancellations.remove(addr) {
            let _ = cancel.send(());
        }
    }
}

impl<F: Future> Stream for MakeStream<F> {
    type Item = (SocketAddr, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            return match self.futures.poll() {
                Err(MakeError::Canceled) => continue,
                Err(MakeError::Inner(err)) => Err(err),
                Ok(Async::Ready(Some((addr, svc)))) => {
                    let _rm = self.cancellations.remove(&addr);
                    debug_assert!(_rm.is_some(), "cancellation missing");
                    Ok(Async::Ready(Some((addr, svc))))
                }
                Ok(r) => Ok(r),
            };
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = (SocketAddr, F::Item);
    type Error = MakeError<F::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Ok(Async::Ready(())) = self.canceled.poll() {
            trace!("canceled making service for {:?}", self.addr);
            return Err(MakeError::Canceled);
        }
        let svc = try_ready!(self.inner.poll());
        Ok((self.addr, svc).into())
    }
}

// === impl MakeError ===

impl<E> From<E> for MakeError<E> {
    fn from(inner: E) -> Self {
        MakeError::Inner(inner)
    }
}
