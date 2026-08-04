use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

const CLIENT_DOMAIN: &[u8] = b"comet-pair-v1/client";
const SERVER_DOMAIN: &[u8] = b"comet-pair-v1/server";
const SESSION_LIFETIME: Duration = Duration::from_secs(5 * 60);
const LIMIT_WINDOW: Duration = Duration::from_secs(60);
const FAILURES_PER_WINDOW: usize = 5;

#[derive(Clone)]
pub struct PairingTranscript {
    bytes: Vec<u8>,
}

impl PairingTranscript {
    pub fn new(
        server_fingerprint: &[u8],
        client_fingerprint: &[u8],
        server_nonce: [u8; 32],
        client_nonce: [u8; 32],
    ) -> Self {
        let mut bytes = Vec::new();
        append_field(&mut bytes, server_fingerprint);
        append_field(&mut bytes, client_fingerprint);
        append_field(&mut bytes, &server_nonce);
        append_field(&mut bytes, &client_nonce);
        Self { bytes }
    }

    pub fn confirm_client(&self, secret: &[u8; 16]) -> [u8; 32] {
        confirmation(secret, CLIENT_DOMAIN, &self.bytes)
    }

    pub fn confirm_server(&self, secret: &[u8; 16]) -> [u8; 32] {
        confirmation(secret, SERVER_DOMAIN, &self.bytes)
    }

    pub fn verify_client(&self, secret: &[u8; 16], tag: &[u8; 32]) -> bool {
        bool::from(self.confirm_client(secret).ct_eq(tag))
    }

    pub fn verify_server(&self, secret: &[u8; 16], tag: &[u8; 32]) -> bool {
        bool::from(self.confirm_server(secret).ct_eq(tag))
    }
}

fn append_field(out: &mut Vec<u8>, field: &[u8]) {
    let length = u32::try_from(field.len()).expect("pairing transcript field fits in u32");
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(field);
}

fn confirmation(secret: &[u8; 16], domain: &[u8], transcript: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("fixed key length");
    mac.update(domain);
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

pub struct PairingSession {
    secret: Zeroizing<[u8; 16]>,
    expires_at: Instant,
    consumed: bool,
    limiter: PairingLimiter,
    generation: u64,
}

impl PairingSession {
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub fn new_at(now: Instant) -> Self {
        Self {
            secret: Zeroizing::new(rand::random()),
            expires_at: now + SESSION_LIFETIME,
            consumed: false,
            limiter: PairingLimiter::default(),
            generation: rand::random(),
        }
    }

    pub fn secret(&self) -> &[u8; 16] {
        &self.secret
    }

    pub fn encoded_secret(&self) -> String {
        let compact = BASE32_NOPAD.encode(self.secret.as_ref());
        compact
            .as_bytes()
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("base32 is ASCII"))
            .collect::<Vec<_>>()
            .join("-")
    }

    pub fn verify_client(
        &mut self,
        transcript: &PairingTranscript,
        tag: &[u8; 32],
        now: Instant,
    ) -> bool {
        self.verify_and_confirm(transcript, tag, now).is_some()
    }

    pub fn is_active(&self, now: Instant) -> bool {
        !self.consumed && now < self.expires_at
    }

    pub(crate) fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub fn expire_if_needed(&mut self, now: Instant) -> bool {
        if !self.consumed && now >= self.expires_at {
            self.consumed = true;
            self.secret.zeroize();
        }
        !self.consumed
    }

    pub fn verify_from(
        &mut self,
        source: IpAddr,
        transcript: &PairingTranscript,
        tag: &[u8; 32],
        now: Instant,
    ) -> PairingAttempt {
        if !self.expire_if_needed(now) {
            return PairingAttempt::Inactive;
        }
        if self.limiter.is_limited(source, now) {
            return PairingAttempt::Limited;
        }
        match self.verify_and_confirm(transcript, tag, now) {
            Some(server_tag) => PairingAttempt::Accepted(server_tag),
            None => {
                self.limiter.record_failure(source, now);
                PairingAttempt::Rejected
            }
        }
    }

    fn verify_and_confirm(
        &mut self,
        transcript: &PairingTranscript,
        tag: &[u8; 32],
        now: Instant,
    ) -> Option<[u8; 32]> {
        if !self.expire_if_needed(now) || !transcript.verify_client(&self.secret, tag) {
            return None;
        }
        let server_tag = transcript.confirm_server(&self.secret);
        self.consumed = true;
        self.secret.zeroize();
        Some(server_tag)
    }
}

impl Default for PairingSession {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingLimit {
    Allowed,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingAttempt {
    Accepted([u8; 32]),
    Rejected,
    Limited,
    Inactive,
}

impl PairingLimit {
    pub fn is_allowed(self) -> bool {
        self == Self::Allowed
    }

    pub fn is_limited(self) -> bool {
        self == Self::Limited
    }
}

#[derive(Default)]
pub struct PairingLimiter {
    failures: HashMap<IpAddr, VecDeque<Instant>>,
}

impl PairingLimiter {
    pub fn is_limited(&mut self, source: IpAddr, now: Instant) -> bool {
        let failures = self.failures.entry(source).or_default();
        prune_failures(failures, now);
        failures.len() >= FAILURES_PER_WINDOW
    }

    pub fn record_failure(&mut self, source: IpAddr, now: Instant) -> PairingLimit {
        let failures = self.failures.entry(source).or_default();
        prune_failures(failures, now);
        if failures.len() >= FAILURES_PER_WINDOW {
            return PairingLimit::Limited;
        }
        failures.push_back(now);
        PairingLimit::Allowed
    }
}

fn prune_failures(failures: &mut VecDeque<Instant>, now: Instant) {
    while failures
        .front()
        .is_some_and(|failure| now.duration_since(*failure) >= LIMIT_WINDOW)
    {
        failures.pop_front();
    }
}
