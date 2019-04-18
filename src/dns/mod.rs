extern crate trust_dns_resolver;
extern crate untrusted;
extern crate webpki;

use self::trust_dns_resolver::{
    lookup_ip::LookupIp, system_conf, AsyncResolver, BackgroundLookupIp,
};
use convert::TryFrom;
use futures::prelude::*;
use std::time::Instant;
use std::{fmt, net};
use tokio::timer::Delay;

mod name;

pub use self::name::{InvalidName, Name};
pub use self::trust_dns_resolver::config::{ResolverOpts, ResolverConfig};
pub use self::trust_dns_resolver::error::{ResolveError, ResolveErrorKind};

#[derive(Clone)]
pub struct Resolver {
    resolver: AsyncResolver,
}

pub trait ConfigureResolver {
    fn configure_resolver(&self, &mut ResolverOpts);
}

pub trait NewResolver {
    type Resolver: Resolve + Refine;
    type Background: Future<Item=(), Error=()> + Send + 'static;
    /// Construct a new `Resolver` from environment variables and system
    /// configuration.
    ///
    /// # Returns
    ///
    /// Either a tuple containing a new `Resolver` and the background task to
    /// drive that resolver's futures, or an error if the system configuration
    /// could not be parsed.
    ///
    /// TODO: This should be infallible like it is in the `domain` crate.
    fn from_system_config_with<C: ConfigureResolver>(
        &self, c: &C,
    ) -> Result<(Self::Resolver, Self::Background), ResolveError> {
        let (config, mut opts) = system_conf::read_system_conf()?;
        c.configure_resolver(&mut opts);
        trace!("DNS config: {:?}", &config);
        trace!("DNS opts: {:?}", &opts);
        Ok(self.new_resolver(config, opts))
    }

    /// NOTE: It would be nice to be able to return a named type rather than
    ///       `impl Future` for the background future; it would be called
    ///       `Background` or `ResolverBackground` if that were possible.
    fn new_resolver(
        &self,
        config: ResolverConfig,
        opts: ResolverOpts,
    ) -> (Self::Resolver, Self::Background);
}

pub trait Refine {
    type Future: Future<Item = RefinedName, Error = ResolveError>  + Send + 'static;
    /// Attempts to refine `name` to a fully-qualified name.
    ///
    /// This method does DNS resolution for `name` and ignores the IP address
    /// result, instead returning the `Name` that was resolved.
    ///
    /// For example, a name like `web` may be refined to `web.example.com.`,
    /// depending on the DNS search path.
    fn refine(&self, name: &Name) -> Self::Future;
}

pub trait Resolve {
    type Future: Future<Item = net::IpAddr, Error = Error> + Send + 'static;
    type List: for<'a> IpList<'a>;
    type ListFuture: Future<Item = Response<Self::List>, Error = ResolveError> + Send + 'static;
    fn resolve_one_ip(&self, name: &Name) -> Self::Future;
    fn resolve_all_ips(&self, deadline: Instant, name: &Name) -> Self::ListFuture;
}

pub trait IpList<'a>: fmt::Debug {
    type Iter: Iterator<Item = net::IpAddr>;
    fn iter(&'a self) -> Self::Iter;
    /// Returns the `Instant` at which this lookup is no longer valid.
    fn valid_until(&self) -> Instant;
}


#[derive(Debug)]
pub enum Error {
    NoAddressesFound,
    ResolutionFailed(ResolveError),
}

pub enum Response<T> {
    Exists(T),
    DoesNotExist { retry_after: Option<Instant> },
}

pub struct IpAddrFuture(::logging::ContextualFuture<Ctx, BackgroundLookupIp>);

pub struct RefineFuture(::logging::ContextualFuture<Ctx, BackgroundLookupIp>);

pub type IpAddrListFuture = Box<Future<Item = Response<LookupIp>, Error = ResolveError> + Send>;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Suffix {
    Root, // The `.` suffix.
    Name(Name),
}

struct Ctx(Name);

pub struct RefinedName {
    pub name: Name,
    pub valid_until: Instant,
}

#[derive(Debug)]
pub struct DefaultResolver;

impl fmt::Display for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "dns={}", self.0)
    }
}

impl fmt::Display for Suffix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Suffix::Root => write!(f, "."),
            Suffix::Name(n) => n.fmt(f),
        }
    }
}

impl From<Name> for Suffix {
    fn from(n: Name) -> Self {
        Suffix::Name(n)
    }
}

impl<'s> TryFrom<&'s str> for Suffix {
    type Err = <Name as TryFrom<&'s [u8]>>::Err;
    fn try_from(s: &str) -> Result<Self, Self::Err> {
        if s == "." {
            Ok(Suffix::Root)
        } else {
            Name::try_from(s.as_bytes()).map(|n| n.into())
        }
    }
}

impl Suffix {
    pub fn contains(&self, name: &Name) -> bool {
        match self {
            Suffix::Root => true,
            Suffix::Name(ref sfx) => {
                let name = name.without_trailing_dot();
                let sfx = sfx.without_trailing_dot();
                name.ends_with(sfx) && {
                    name.len() == sfx.len() || {
                        // foo.bar.bah (11)
                        // bar.bah (7)
                        let idx = name.len() - sfx.len();
                        let (hd, _) = name.split_at(idx);
                        hd.ends_with('.')
                    }
                }
            }
        }
    }
}

impl NewResolver for DefaultResolver {
    type Resolver = Resolver;
    type Background = Box<Future<Item = (), Error = ()> + Send + 'static>;

    /// NOTE: It would be nice to be able to return a named type rather than
    ///       `impl Future` for the background future; it would be called
    ///       `Background` or `ResolverBackground` if that were possible.
    fn new_resolver(
        &self,
        config: ResolverConfig,
        mut opts: ResolverOpts,
    ) -> (Self::Resolver, Self::Background) {
        // Disable Trust-DNS's caching.
        opts.cache_size = 0;
        let (resolver, background) = AsyncResolver::new(config, opts);
        let resolver = Resolver { resolver };
        (resolver, Box::new(background))
    }
}

impl Resolve for Resolver {
    type Future = IpAddrFuture;
    type List = LookupIp;
    type ListFuture = Box<Future<Item = Response<Self::List>, Error = ResolveError> + Send + 'static>;

    fn resolve_all_ips(&self, deadline: Instant, name: &Name) -> Self::ListFuture {
        let lookup = self.resolver.lookup_ip(name.as_ref());

        // FIXME this delay logic is really confusing...
        let f = Delay::new(deadline)
            .then(move |_| {
                trace!("after delay");
                lookup
            })
            .then(move |result| {
                trace!("completed with {:?}", &result);
                result.map(Response::Exists).or_else(|e| {
                    if let &ResolveErrorKind::NoRecordsFound { valid_until, .. } = e.kind() {
                        Ok(Response::DoesNotExist {
                            retry_after: valid_until,
                        })
                    } else {
                        Err(e)
                    }
                })
            });

        Box::new(::logging::context_future(Ctx(name.clone()), f))
    }

    fn resolve_one_ip(&self, name: &Name) -> Self::Future {
        let f = self.resolver.lookup_ip(name.as_ref());
        IpAddrFuture(::logging::context_future(Ctx(name.clone()), f))
    }
}

impl Refine for Resolver {
    type Future = RefineFuture;

    fn refine(&self, name: &Name) -> Self::Future {
        let f = self.resolver.lookup_ip(name.as_ref());
        RefineFuture(::logging::context_future(Ctx(name.clone()), f))
    }
}

/// Note: `AsyncResolver` does not implement `Debug`, so we must manually
///       implement this.
impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("resolver", &"...")
            .finish()
    }
}

impl Future for IpAddrFuture {
    type Item = net::IpAddr;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ips = try_ready!(self.0.poll().map_err(Error::ResolutionFailed));
        ips.iter()
            .next()
            .map(Async::Ready)
            .ok_or_else(|| Error::NoAddressesFound)
    }
}

impl Future for RefineFuture {
    type Item = RefinedName;
    type Error = ResolveError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let lookup = try_ready!(self.0.poll());
        let valid_until = lookup.valid_until();

        let n = lookup.query().name();
        let name = Name::try_from(n.to_ascii().as_bytes())
            .expect("Name returned from resolver must be valid");

        let refine = RefinedName { name, valid_until };
        Ok(Async::Ready(refine))
    }
}

impl<'a> IpList<'a> for LookupIp {
    type Iter = trust_dns_resolver::lookup_ip::LookupIpIter<'a>;
    fn iter(&'a self) -> Self::Iter {
        LookupIp::iter(self)
    }
    /// Returns the `Instant` at which this lookup is no longer valid.
    fn valid_until(&self) -> Instant {
        LookupIp::valid_until(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{Name, Suffix};
    use convert::TryFrom;

    #[test]
    fn test_dns_name_parsing() {
        // Stack sure `dns::Name`'s validation isn't too strict. It is
        // implemented in terms of `webpki::DNSName` which has many more tests
        // at https://github.com/briansmith/webpki/blob/master/tests/dns_name_tests.rs.

        struct Case {
            input: &'static str,
            output: &'static str,
        }

        static VALID: &[Case] = &[
            // Almost all digits and dots, similar to IPv4 addresses.
            Case {
                input: "1.2.3.x",
                output: "1.2.3.x",
            },
            Case {
                input: "1.2.3.x",
                output: "1.2.3.x",
            },
            Case {
                input: "1.2.3.4A",
                output: "1.2.3.4a",
            },
            Case {
                input: "a.b.c.d",
                output: "a.b.c.d",
            },
            // Uppercase letters in labels
            Case {
                input: "A.b.c.d",
                output: "a.b.c.d",
            },
            Case {
                input: "a.mIddle.c",
                output: "a.middle.c",
            },
            Case {
                input: "a.b.c.D",
                output: "a.b.c.d",
            },
            // Absolute
            Case {
                input: "a.b.c.d.",
                output: "a.b.c.d.",
            },
        ];

        for case in VALID {
            let name = Name::try_from(case.input.as_bytes());
            assert_eq!(name.as_ref().map(|x| x.as_ref()), Ok(case.output));
        }

        static INVALID: &[&str] = &[
            // These are not in the "preferred name syntax" as defined by
            // https://tools.ietf.org/html/rfc1123#section-2.1. In particular
            // the last label only has digits.
            "1.2.3.4", "a.1.2.3", "1.2.x.3",
        ];

        for case in INVALID {
            assert!(Name::try_from(case.as_bytes()).is_err());
        }
    }

    #[test]
    fn suffix_valid() {
        for (name, suffix) in &[
            ("a", "."),
            ("a.", "."),
            ("a.b", "."),
            ("a.b.", "."),
            ("b.c", "b.c"),
            ("b.c", "b.c"),
            ("a.b.c", "b.c"),
            ("a.b.c", "b.c."),
            ("a.b.c.", "b.c"),
            ("hacker.example.com", "example.com"),
        ] {
            let n = Name::try_from(name.as_bytes()).unwrap();
            let s = Suffix::try_from(suffix).unwrap();
            assert!(
                s.contains(&n),
                format!("{} should contain {}", suffix, name)
            );
        }
    }

    #[test]
    fn suffix_invalid() {
        for (name, suffix) in &[
            ("a", "b"),
            ("b", "a.b"),
            ("b.a", "b"),
            ("hackerexample.com", "example.com"),
        ] {
            let n = Name::try_from(name.as_bytes()).unwrap();
            let s = Suffix::try_from(suffix).unwrap();
            assert!(
                !s.contains(&n),
                format!("{} should not contain {}", suffix, name)
            );
        }

        assert!(Suffix::try_from("").is_err(), "suffix must not be empty");
    }
}
