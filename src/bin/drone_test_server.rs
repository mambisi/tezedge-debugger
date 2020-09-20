// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use tezedge_debugger::utility::{
    identity::Identity,
    stream::MessageStream,
};
use tokio::{
    net::{TcpListener, TcpStream},
};
use lazy_static::lazy_static;
use crypto::nonce::{Nonce, NoncePair, generate_nonces};
use tezos_messages::p2p::encoding::connection::ConnectionMessage;
use tezos_messages::p2p::binary_message::{BinaryChunk, BinaryMessage};
use crypto::crypto_box::precompute;
use tezedge_debugger::utility::stream::{EncryptedMessageWriter, EncryptedMessageReader, StreamError};
use tezos_messages::p2p::encoding::peer::{PeerMessageResponse};
use std::net::{SocketAddr};
use std::convert::TryFrom;
use tezos_messages::p2p::encoding::metadata::MetadataMessage;
use tezos_messages::p2p::encoding::ack::AckMessage;

lazy_static! {
    static ref IDENTITY: Identity = Identity {
        peer_id: "idsscFHxXoeJjxQsQBeEveayLyvymA".to_string(),
        public_key: "b41df26473332e7225fdad07045112b5ba6bf295a384785c535cf738575ee245".to_string(),
        secret_key: "dc9640dbd8cf50a5475b6a6d65c96af943380a627cea198906a2a8d4fd37decc".to_string(),
        proof_of_work_stamp: "d0e1945cb693c743e82b3e29750ebbc746c14dbc280c6ee6".to_string(),
    };

    static ref NONCE: Nonce = Nonce::random();
}

/// This is server handler, all connection will *ALWAYS* be incoming, from some running drone-client
/// Simple and naive ping server, everything will be sent back without any processing.
/// This way, it should ensure correct Tezos Handshake and correct encodings. Which means, only
/// client should be responsible for correct encryption (of his side, as server will just mirror it).
async fn handle_stream(stream: TcpStream, peer_addr: SocketAddr) {
    println!("[{}] Spawned peer handler", peer_addr);

    let (mut reader, mut writer) = MessageStream::from(stream).split();

    let recv_chunk = reader.read_message().await.unwrap();
    let recv_conn_msg = ConnectionMessage::try_from(recv_chunk).unwrap();

    println!("[{}] Received connection message", peer_addr);

    let sent_conn_msg = ConnectionMessage::new(
        0,
        &IDENTITY.public_key,
        &IDENTITY.proof_of_work_stamp,
        &NONCE.get_bytes(),
        Default::default(),
    );
    let sent_chunk = BinaryChunk::from_content(&sent_conn_msg.as_bytes().unwrap()).unwrap();
    writer.write_message(&sent_chunk)
        .await.unwrap();

    let sent_data = BinaryChunk::from_content(&sent_conn_msg.as_bytes().unwrap()).unwrap();
    let recv_data = BinaryChunk::from_content(&recv_conn_msg.as_bytes().unwrap()).unwrap();

    let precomputed_key = precompute(
        &hex::encode(recv_conn_msg.public_key),
        &IDENTITY.secret_key,
    ).unwrap();

    let NoncePair { remote, local } = generate_nonces(
        sent_data.raw(),
        recv_data.raw(),
        true,
    );

    println!(
        "[{}] Encrypted connection\n\tsent={:?}\n\trecv={:?}\n\tlocal={:?}\n\tremote={:?}\n\tpk={}",
        peer_addr,
        sent_data.raw(),
        recv_data.raw(),
        &local, &remote,
        hex::encode(precomputed_key.as_ref().as_ref())
    );

    let mut enc_writer = EncryptedMessageWriter::new(writer, precomputed_key.clone(), local, IDENTITY.peer_id.clone());
    let mut enc_reader = EncryptedMessageReader::new(reader, precomputed_key.clone(), remote, IDENTITY.peer_id.clone());

    let metadata = enc_reader.read_message::<MetadataMessage>().await.unwrap();
    println!("[{}] Decrypted metadata message", peer_addr);
    enc_writer.write_message(&metadata).await.unwrap();

    let ack = enc_reader.read_message::<AckMessage>().await.unwrap();
    println!("[{}] Decrypted ack message", peer_addr);
    enc_writer.write_message(&ack).await.unwrap();

    loop {
        match enc_reader.read_message::<PeerMessageResponse>().await {
            Ok(message) => {
                println!("[{}] Decrypted message", peer_addr);
                enc_writer.write_message(&message).await.unwrap();
                println!("[{}] Sent re-encrypted message", peer_addr);
            }
            Err(err) => {
                match err {
                    StreamError::FailedToEncryptMessage { .. } => {
                        eprintln!("[{}] Failed to encrypt message: {:?}", peer_addr, err)
                    }
                    StreamError::FailedToDecryptMessage { .. } => {
                        eprintln!("[{}] Failed to decrypt message: {:?}", peer_addr, err)
                    }
                    StreamError::SerializationError { .. } => {
                        eprintln!("[{}] Failed to serialize message: {:?}", peer_addr, err)
                    }
                    StreamError::DeserializationError { .. } => {
                        eprintln!("[{}] Failed to deserialize message: {:?}", peer_addr, err)
                    }
                    StreamError::NetworkError { .. } => {
                        println!("[{}] Closing connection", peer_addr);
                        return;
                    }
                }
            }
        }
    }
}

#[tokio::main]
/// Build trivial TCP server with ping handler
pub async fn main() -> std::io::Result<()> {
    let server = "0.0.0.0:13030";
    let mut listener = TcpListener::bind(server).await?;
    println!("Started to listening on \"{}\"", server);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        tokio::spawn(handle_stream(stream, peer_addr));
    }
}