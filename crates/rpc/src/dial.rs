//! Happy Eyeballs TCP dialing for direct LAN connections.
//!
//! Some networks publish both address families but blackhole one of them. A
//! sequential dial can then wait for the operating system's TCP timeout even
//! though another resolved address is reachable. This module starts one new
//! attempt every 250 ms, or immediately after a failure, and keeps the first
//! successful stream.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::net::{TcpStream, ToSocketAddrs};

const HAPPY_EYEBALLS_STAGGER: Duration = Duration::from_millis(250);

/// Resolve and race TCP candidates before the caller begins TLS negotiation.
pub(super) async fn connect_tcp<A>(endpoint: A) -> io::Result<TcpStream>
where
    A: ToSocketAddrs,
{
    let addrs = tokio::net::lookup_host(endpoint).await?.collect();
    let stream = race_connect_with(interleave_families(addrs), TcpStream::connect, || {
        tokio::time::sleep(HAPPY_EYEBALLS_STAGGER)
    })
    .await?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

fn interleave_families(addrs: Vec<SocketAddr>) -> VecDeque<SocketAddr> {
    let first_is_ipv6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (preferred, alternate): (Vec<_>, Vec<_>) = addrs
        .into_iter()
        .partition(|address| address.is_ipv6() == first_is_ipv6);
    let mut preferred = preferred.into_iter();
    let mut alternate = alternate.into_iter();
    let mut ordered = VecDeque::with_capacity(preferred.len() + alternate.len());

    loop {
        match (preferred.next(), alternate.next()) {
            (None, None) => return ordered,
            (preferred, alternate) => {
                ordered.extend(preferred);
                ordered.extend(alternate);
            }
        }
    }
}

async fn race_connect_with<T, Connect, ConnectFuture, Wait, WaitFuture>(
    mut queue: VecDeque<SocketAddr>,
    mut connect: Connect,
    mut wait: Wait,
) -> io::Result<T>
where
    Connect: FnMut(SocketAddr) -> ConnectFuture,
    ConnectFuture: Future<Output = io::Result<T>>,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    let first = queue
        .pop_front()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no addresses resolved"))?;
    let mut pending = FuturesUnordered::new();
    pending.push(connect(first));

    loop {
        tokio::select! {
            result = pending.next(), if !pending.is_empty() => match result {
                Some(Ok(winner)) => return Ok(winner),
                Some(Err(error)) => {
                    if let Some(address) = queue.pop_front() {
                        pending.push(connect(address));
                    } else if pending.is_empty() {
                        return Err(error);
                    }
                }
                None => unreachable!("pending branch is guarded"),
            },
            () = wait(), if !queue.is_empty() => {
                let address = queue.pop_front().expect("wait branch is guarded");
                pending.push(connect(address));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use tokio::sync::{Notify, mpsc};
    use tokio::time::{Duration, timeout};

    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().expect("literal socket address")
    }

    #[test]
    fn interleaves_families_from_resolver_preference() {
        let ipv6_first = interleave_families(vec![
            addr("[2001:db8::1]:443"),
            addr("[2001:db8::2]:443"),
            addr("192.0.2.1:443"),
            addr("192.0.2.2:443"),
            addr("192.0.2.3:443"),
        ]);
        assert_eq!(
            ipv6_first.into_iter().collect::<Vec<_>>(),
            vec![
                addr("[2001:db8::1]:443"),
                addr("192.0.2.1:443"),
                addr("[2001:db8::2]:443"),
                addr("192.0.2.2:443"),
                addr("192.0.2.3:443"),
            ]
        );

        let ipv4_first = interleave_families(vec![
            addr("192.0.2.1:443"),
            addr("192.0.2.2:443"),
            addr("[2001:db8::1]:443"),
            addr("[2001:db8::2]:443"),
        ]);
        assert_eq!(
            ipv4_first.into_iter().collect::<Vec<_>>(),
            vec![
                addr("192.0.2.1:443"),
                addr("[2001:db8::1]:443"),
                addr("192.0.2.2:443"),
                addr("[2001:db8::2]:443"),
            ]
        );
    }

    #[tokio::test]
    async fn stalled_first_attempt_does_not_block_the_working_family() {
        let stalled = addr("[2001:db8::1]:443");
        let working = addr("192.0.2.1:443");
        let (attempt_tx, mut attempts) = mpsc::unbounded_channel();
        let release_stagger = Arc::new(Notify::new());
        let wait_gate = release_stagger.clone();

        let race = tokio::spawn(race_connect_with(
            VecDeque::from([stalled, working]),
            move |address| {
                let attempt_tx = attempt_tx.clone();
                async move {
                    attempt_tx.send(address).expect("attempt observer remains");
                    if address == stalled {
                        future::pending().await
                    } else {
                        Ok(address)
                    }
                }
            },
            move || {
                let wait_gate = wait_gate.clone();
                async move { wait_gate.notified().await }
            },
        ));

        assert_eq!(
            timeout(Duration::from_secs(1), attempts.recv())
                .await
                .expect("first attempt starts")
                .expect("attempt channel stays open"),
            stalled
        );
        assert!(
            matches!(attempts.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "alternate family started before the stagger elapsed"
        );

        release_stagger.notify_one();
        assert_eq!(
            timeout(Duration::from_secs(1), attempts.recv())
                .await
                .expect("alternate attempt starts after the stagger")
                .expect("attempt channel stays open"),
            working
        );
        let winner = timeout(Duration::from_secs(1), race)
            .await
            .expect("race completes")
            .expect("race task remains healthy")
            .expect("one address connects");
        assert_eq!(winner, working);
    }

    #[tokio::test]
    async fn failed_attempt_advances_without_waiting_for_the_stagger() {
        let refused = addr("[2001:db8::1]:443");
        let working = addr("192.0.2.1:443");
        let never_release = Arc::new(Notify::new());
        let wait_gate = never_release.clone();

        let winner = timeout(
            Duration::from_secs(1),
            race_connect_with(
                VecDeque::from([refused, working]),
                move |address| async move {
                    if address == refused {
                        Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "first address refused",
                        ))
                    } else {
                        Ok(address)
                    }
                },
                move || {
                    let wait_gate = wait_gate.clone();
                    async move { wait_gate.notified().await }
                },
            ),
        )
        .await
        .expect("a refusal advances the queue without the wait gate")
        .expect("the second address connects");

        assert_eq!(winner, working);
    }

    #[tokio::test]
    async fn all_failed_attempts_return_the_last_connection_error() {
        let first = addr("[2001:db8::1]:443");
        let second = addr("192.0.2.1:443");
        let error = race_connect_with(
            VecDeque::from([first, second]),
            move |address| async move {
                let kind = if address == first {
                    io::ErrorKind::ConnectionRefused
                } else {
                    io::ErrorKind::TimedOut
                };
                Err::<SocketAddr, _>(io::Error::new(kind, "controlled failure"))
            },
            || async {},
        )
        .await
        .expect_err("every connection attempt failed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn empty_resolution_is_not_found() {
        let error = race_connect_with(
            VecDeque::new(),
            |_address| future::ready(Ok::<_, io::Error>(())),
            || future::ready(()),
        )
        .await
        .expect_err("an empty resolver result cannot connect");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
