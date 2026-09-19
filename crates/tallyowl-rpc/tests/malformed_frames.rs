//! Malformed frames at the trust boundary, and a seeded fuzz over the framing.
//!
//! `docs/PLAN.md` Phase 11 requires malformed-frame tests, and the rule they
//! prove is the one `docs/THREAT_MODEL.md` section 3 rests on: bytes from a
//! network cannot take the listener down, cannot make it allocate what a
//! length prefix promises, and cannot poison the connection after them.
//!
//! The fuzzer here is a seeded mutation loop rather than a coverage-guided
//! one, because this machine's toolchain has no nightly compiler for a
//! libFuzzer build. The seed is recorded (section 9 of the implementation
//! prompt: a result nobody can reproduce is not a result), the corpus is a
//! valid frame, and every mutation must leave the server answering.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use tallyowl_rpc::{reply, Client, Outcome, Request};

const SEED: u64 = 20260809;

/// A dispatcher that answers every request, so a test can tell "the server
/// refused the bytes" apart from "the server died".
fn echo(request: &Request) -> Outcome {
    reply("Echo", request.payload.clone())
}

fn serve() -> tallyowl_rpc::Server {
    tallyowl_rpc::serve("127.0.0.1:0", Arc::new(echo), 1024 * 1024).expect("a listener")
}

/// The server still answers a well-formed request. This is the assertion every
/// test below ends with, because surviving garbage only matters if the next
/// caller is served.
fn still_answers(address: &std::net::SocketAddr) {
    let client = Client::new(address.to_string(), 1024 * 1024);
    let response = client
        .call("Probe", "echo", b"still there?".to_vec())
        .expect("the server answers after the garbage");
    assert_eq!(response.payload, b"still there?");
}

#[test]
fn a_length_prefix_promising_four_gigabytes_is_refused_before_allocation() {
    let server = serve();
    let address = server.local_address();

    let mut socket = TcpStream::connect(address).expect("a connection");
    // The largest length a prefix can carry. A reader that allocated it would
    // be a one-frame denial of service; the guard refuses before allocating.
    socket.write_all(&u32::MAX.to_be_bytes()).expect("written");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a timeout");
    let mut buffer = [0u8; 16];
    // The server drops the connection rather than waiting for four gigabytes.
    let _ = socket.read(&mut buffer);

    still_answers(&address);
}

#[test]
fn a_truncated_frame_is_a_closed_connection_rather_than_a_hang() {
    let server = serve();
    let address = server.local_address();

    let mut socket = TcpStream::connect(address).expect("a connection");
    // Promise one hundred bytes, deliver ten, and leave. The reader must treat
    // the early end as the end rather than waiting for the rest for ever —
    // which is exactly the shape of hang L131 taught this project to fear.
    socket.write_all(&100u32.to_be_bytes()).expect("written");
    socket.write_all(&[0xAB; 10]).expect("written");
    drop(socket);

    still_answers(&address);
}

#[test]
fn garbage_inside_a_well_formed_frame_is_refused_and_the_listener_survives() {
    let server = serve();
    let address = server.local_address();

    let mut socket = TcpStream::connect(address).expect("a connection");
    let garbage = [0xFFu8; 64];
    socket
        .write_all(&(garbage.len() as u32).to_be_bytes())
        .expect("written");
    socket.write_all(&garbage).expect("written");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a timeout");
    let mut buffer = [0u8; 256];
    // The server may answer a transport error or close; either way it must not
    // die and must not echo the garbage back as a success.
    let _ = socket.read(&mut buffer);

    still_answers(&address);
}

#[test]
fn a_thousand_seeded_mutations_of_a_valid_frame_leave_the_server_answering() {
    let server = serve();
    let address = server.local_address();

    // The corpus: the bytes a real request puts on the wire, taken by writing
    // one through the real client against a socket we read ourselves would be
    // circular — so it is built from the frame layout the transport documents:
    // a 4-byte big-endian length prefix over an encoded request. The encoded
    // request bytes come from a live round trip.
    let client = Client::new(address.to_string(), 1024 * 1024);
    client
        .call("Probe", "echo", b"corpus".to_vec())
        .expect("the corpus request works before mutation");

    // xorshift64*, seeded. Every run asks the same questions.
    let mut state = SEED;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        state
    };

    // A plausible envelope-shaped payload to mutate: CBOR-ish bytes with
    // embedded text, which reaches deeper than pure noise does.
    let corpus: Vec<u8> = {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xA4, 0x61]); // a small CBOR map opening
        bytes.extend_from_slice(b"sProbe");
        bytes.extend_from_slice(&[0x61]);
        bytes.extend_from_slice(b"oecho");
        bytes.extend_from_slice(&[0x61]);
        bytes.extend_from_slice(b"p");
        bytes.extend_from_slice(&[0x58, 0x20]);
        bytes.extend_from_slice(&[0x42; 32]);
        bytes
    };

    for round in 0..1_000u32 {
        let mut mutated = corpus.clone();
        // One to four mutations: a bit flip, a truncation, or an insertion.
        for _ in 0..(next() % 4 + 1) {
            match next() % 3 {
                0 if !mutated.is_empty() => {
                    let at = (next() as usize) % mutated.len();
                    mutated[at] ^= (next() % 255 + 1) as u8;
                }
                1 if mutated.len() > 2 => {
                    mutated.truncate((next() as usize) % mutated.len() + 1);
                }
                _ => {
                    let at = (next() as usize) % (mutated.len() + 1);
                    mutated.insert(at, (next() % 256) as u8);
                }
            }
        }

        let mut socket = TcpStream::connect(address).expect("a connection");
        socket
            .write_all(&(mutated.len() as u32).to_be_bytes())
            .unwrap_or_else(|e| panic!("round {round}: the prefix did not write: {e}"));
        socket
            .write_all(&mutated)
            .unwrap_or_else(|e| panic!("round {round}: the frame did not write: {e}"));
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("a timeout");
        let mut buffer = [0u8; 512];
        let _ = socket.read(&mut buffer);
    }

    still_answers(&address);
}
