// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use std::marker::PhantomData;
use either::Either;
use thiserror::Error;
use typenum::{self, Bit};
use super::{
    buffer::Buffer,
    key::{Keys, Key},
    tables::{connection, chunk},
    common::{Sender, Local, Remote},
    Identity,
};

struct Inner<S> {
    cn: connection::Item,
    id: Identity,
    buffer: Buffer,
    incoming: PhantomData<S>,
}

impl<S> Inner<S>
where
    S: Bit,
{
    fn chunk(&self, counter: u64, bytes: Vec<u8>, plain: Vec<u8>) -> chunk::Item {
        chunk::Item::new(self.cn.key(), Sender::new(S::BOOL), counter, bytes, plain)
    }

    pub fn handle_data(&mut self, payload: &[u8]) {
        self.buffer.handle_data(payload);
    }

    pub fn cleanup(&mut self) -> Option<chunk::Item> {
        self.buffer.cleanup()
            .map(|(counter, bytes)| self.chunk(counter, bytes, Vec::new()))
    }
}

/// State machine:
///
///                  Handshake
///     Handshake                         HaveCm           Broken
///                     HaveNotKey         HaveKey         Broken
///               HaveNotKey               HaveData
///                         CannotDecrypt   HaveKey   HaveData
///                           CannotDecrypt

pub struct Initial<S> {
    inner: Inner<S>,
}

pub struct HaveCm<S> {
    inner: Inner<S>,
}

pub struct Uncertain<S> {
    inner: Inner<S>,
}

pub struct HaveKey<S> {
    inner: Inner<S>,
    key: Key,
}

pub struct HaveNotKey<S> {
    inner: Inner<S>,
}

pub struct HaveData<S> {
    inner: Inner<S>,
    key: Key,
    error: Option<u64>,
}

pub struct CannotDecrypt<S> {
    inner: Inner<S>,
}

impl<S> Initial<S>
where
    S: Bit,
{
    pub fn new(cn: connection::Item, id: Identity) -> Self {
        Initial {
            inner: Inner {
                cn,
                id,
                buffer: Buffer::default(),
                incoming: PhantomData,
            },
        }
    }

    pub fn uncertain(self) -> (Uncertain<S>, Option<chunk::Item>) {
        Uncertain::new(self.inner)
    }

    pub fn handle_data(mut self, payload: &[u8]) -> Either<Self, HaveCm<S>> {
        self.inner.handle_data(payload);
        if self.inner.buffer.have_chunk().is_some() {
            Either::Right(HaveCm { inner: self.inner })
        } else {
            Either::Left(self)
        }
    }
}

impl<S> HaveCm<S>
where
    S: Bit,
{
    /// Should not need to call
    pub fn handle_data(mut self, payload: &[u8]) -> Result<Self, (Uncertain<S>, Option<chunk::Item>)> {
        // 128 kiB
        if self.inner.buffer.remaining() > 0x20000 {
            Err(Uncertain::new(self.inner))
        } else {
            self.inner.handle_data(payload);
            Ok(self)
        }
    }

    fn have_key(mut self, key: Key) -> (HaveKey<S>, chunk::Item) {
        let (counter, bytes) = self.inner.buffer.next().unwrap();
        let remaining = self.inner.buffer.remaining();
        if remaining > 0 {
            log::warn!(
                "have {} bytes after connection message received, but before got key",
                remaining,
            );
        }
        let plain = bytes[2..].to_vec();
        let c = self.inner.chunk(counter, bytes, plain);
        (
            HaveKey {
                inner: self.inner,
                key,
            },
            c,
        )
    }

    fn have_not_key(mut self) -> (HaveNotKey<S>, Option<chunk::Item>) {
        let c = self.inner.cleanup();
        (HaveNotKey { inner: self.inner }, c)
    }
}

pub struct MakeKeyOutput {
    pub cn: connection::Item,
    pub local: Result<HaveKey<Local>, HaveNotKey<Local>>,
    pub l_chunk: Option<chunk::Item>,
    pub remote: Result<HaveKey<Remote>, HaveNotKey<Remote>>,
    pub r_chunk: Option<chunk::Item>,
}

impl HaveCm<Local> {
    pub fn make_key(self, peer: HaveCm<Remote>) -> MakeKeyOutput {
        use crypto::proof_of_work;

        #[derive(Error, Debug)]
        pub enum HandshakeWarning {
            #[error("connection message is too short {}", _0)]
            ConnectionMessageTooShort(usize),
            #[error("proof-of-work check failed")]
            PowInvalid(f64),
        }

        let check = |payload: &[u8]| -> Result<[u8; 32], HandshakeWarning> {
            if payload.len() <= 88 {
                return Err(HandshakeWarning::ConnectionMessageTooShort(payload.len()));
            }
            // TODO: move to config
            let target = 26.0;
            if proof_of_work::check_proof_of_work(&payload[4..60], target).is_err() {
                return Err(HandshakeWarning::PowInvalid(target));
            }

            let mut pk = [0; 32];
            pk.clone_from_slice(&payload[4..36]);
            Ok(pk)
        };

        let local_chunk = self.inner.buffer.have_chunk().unwrap();
        let remote_chunk = peer.inner.buffer.have_chunk().unwrap();
        let mut cn = self.inner.cn.clone();
        let identity = &self.inner.id;
        match Keys::new(
            identity,
            local_chunk,
            remote_chunk,
            self.inner.cn.initiator.clone(),
        ) {
            Ok(Keys { local, remote }) => {
                let (l, l_chunk) = self.have_key(local);
                let (r, r_chunk) = peer.have_key(remote);
                match check(&l_chunk.bytes) {
                    Ok(_) => (),
                    Err(HandshakeWarning::ConnectionMessageTooShort(size)) => {
                        cn.add_comment().outgoing_too_short = Some(size);
                    },
                    Err(HandshakeWarning::PowInvalid(target)) => {
                        cn.add_comment().outgoing_wrong_pow = Some(target);
                    },
                }
                match check(&r_chunk.bytes) {
                    Ok(peer_pk) => cn.set_peer_pk(peer_pk),
                    Err(HandshakeWarning::ConnectionMessageTooShort(size)) => {
                        cn.add_comment().incoming_too_short = Some(size);
                    },
                    Err(HandshakeWarning::PowInvalid(target)) => {
                        cn.add_comment().incoming_wrong_pow = Some(target);
                    },
                }
                MakeKeyOutput {
                    cn,
                    local: Ok(l),
                    l_chunk: Some(l_chunk),
                    remote: Ok(r),
                    r_chunk: Some(r_chunk),
                }
            },
            Err(_) => {
                let (l, l_chunk) = self.have_not_key();
                let (r, r_chunk) = peer.have_not_key();
                cn.add_comment().outgoing_wrong_pk = true;
                MakeKeyOutput {
                    cn,
                    local: Err(l),
                    l_chunk,
                    remote: Err(r),
                    r_chunk,
                }
            },
        }
    }
}

impl<S> Uncertain<S>
where
    S: Bit,
{
    fn new(mut inner: Inner<S>) -> (Uncertain<S>, Option<chunk::Item>) {
        let c= inner.cleanup();
        let cn_value = match serde_json::to_string(&inner.cn.value()) {
            Ok(s) => s,
            Err(s) => format!("{:?}", s),
        };
        log::warn!(
            "uncertain connection: {}, {}",
            cn_value,
            inner.cn.key(),
        );
        if S::BOOL {
            inner.cn.add_comment().incoming_uncertain = true;
        } else {
            inner.cn.add_comment().outgoing_uncertain = true;
        }
        (Uncertain { inner }, c)
    }

    pub fn handle_data(&mut self, payload: &[u8]) -> chunk::Item {
        debug_assert!(!payload.is_empty());
        self.inner.handle_data(payload);
        self.inner.cleanup().unwrap()
    }

    pub fn cn(&self) -> connection::Item {
        self.inner.cn.clone()
    }
}

impl<S> HaveNotKey<S>
where
    S: Bit,
{
    pub fn handle_data(&mut self, payload: &[u8]) -> chunk::Item {
        debug_assert!(!payload.is_empty());
        self.inner.handle_data(payload);
        self.inner.cleanup().unwrap()
    }
}

impl<S> HaveKey<S>
where
    S: Bit,
{
    pub fn handle_data(mut self, payload: &[u8]) -> HaveData<S> {
        self.inner.handle_data(payload);
        HaveData {
            inner: self.inner,
            key: self.key,
            error: None,
        }
    }
}

impl<S> Iterator for HaveData<S>
where
    S: Bit,
{
    type Item = chunk::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.is_some() {
            return None;
        }
        let (counter, bytes) = self.inner.buffer.next()?;
        match self.key.decrypt(&bytes) {
            Ok(plain) => Some(self.inner.chunk(counter, bytes, plain)),
            Err(_) => {
                self.error = Some(counter);
                let cn_value = match serde_json::to_string(&self.inner.cn.value()) {
                    Ok(s) => s,
                    Err(s) => format!("{:?}", s),
                };
                log::warn!(
                    "cannot decrypt: {}-{}-{}, connection: {}",
                    self.inner.cn.key(),
                    Sender::new(S::BOOL),
                    counter,
                    cn_value,
                );
                self.inner.cleanup()
            },
        }
    }
}

impl<S> HaveData<S>
where
    S: Bit,
{
    pub fn over(self) -> Result<HaveKey<S>, (CannotDecrypt<S>, connection::Item)> {
        if let Some(position) = self.error {
            let mut inner = self.inner;
            if S::BOOL {
                inner.cn.add_comment().incoming_cannot_decrypt = Some(position);
            } else {
                inner.cn.add_comment().outgoing_cannot_decrypt = Some(position);
            }
            let cn = inner.cn.clone();
            Err((CannotDecrypt { inner }, cn))
        } else {
            Ok(HaveKey {
                inner: self.inner,
                key: self.key,
            })
        }
    }
}

impl<S> CannotDecrypt<S>
where
    S: Bit,
{
    pub fn handle_data(&mut self, payload: &[u8]) -> chunk::Item {
        debug_assert!(!payload.is_empty());
        self.inner.handle_data(payload);
        self.inner.cleanup().unwrap()
    }
}