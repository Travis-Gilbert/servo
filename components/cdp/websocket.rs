/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A small WebSocket implementation for CDP text frames.
//!
//! The server accepts one HTTP request per connection. A discovery `GET`
//! is answered in place. A WebSocket upgrade is answered, and later frames
//! are unmasked text. Payloads are capped so a client cannot make the
//! process buffer without limit.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha1::{Digest, Sha1};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

pub(crate) fn read_http_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "http header"));
        }
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "http header"));
        }
    }
    let text = String::from_utf8(buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut lines = text.split("\r\n");
    let Some(start) = lines.next() else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "http start"));
    };
    let mut parts = start.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
    })
}

pub(crate) fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| value.as_str())
}

pub(crate) fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(GUID.as_bytes());
    STANDARD.encode(hasher.finalize())
}

pub(crate) fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub(crate) fn write_upgrade(stream: &mut TcpStream, accept: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

pub(crate) fn write_text(stream: &mut TcpStream, text: &str) -> io::Result<()> {
    let payload = text.as_bytes();
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame"));
    }
    let mut header = Vec::with_capacity(10);
    header.push(0x81);
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

pub(crate) fn read_text(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut collected = Vec::new();
    loop {
        let (opcode, fin, payload) = read_frame(stream)?;
        if opcode == 0x8 {
            return Ok(None);
        }
        if opcode == 0x9 {
            write_pong(stream, &payload)?;
            continue;
        }
        if opcode != 0x1 && opcode != 0x0 {
            continue;
        }
        collected.extend(payload);
        if collected.len() > MAX_PAYLOAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame"));
        }
        if fin {
            return Ok(Some(String::from_utf8(collected).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, error)
            })?));
        }
    }
}

fn write_pong(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&[0x8A, payload.len() as u8])?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(stream: &mut TcpStream) -> io::Result<(u8, bool, Vec<u8>)> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header)?;
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7F) as usize;
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended)?;
        length = u16::from_be_bytes(extended) as usize;
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended) as usize;
    }
    if length > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame"));
    }
    let mask = if masked {
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask)?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((opcode, fin, payload))
}

pub(crate) fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'(' | b')' | b',')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn text_round_trip_and_path_encoding() {
        assert_eq!(decode_segment("a%20b"), "a b");
        assert_eq!(encode_segment("a b"), "a%20b");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream).unwrap();
            assert_eq!(request.path, "/devtools/page/a%20b");
            let key = header(&request, "sec-websocket-key").unwrap();
            write_upgrade(&mut stream, &accept_key(key)).unwrap();
            let text = read_text(&mut stream).unwrap().unwrap();
            assert_eq!(text, "hello");
            write_text(&mut stream, "world").unwrap();
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            client,
            "GET /devtools/page/a%20b HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .unwrap();
        let response = read_http_request_raw(&mut client);
        assert!(response.starts_with("HTTP/1.1 101"));
        write_client_text(&mut client, "hello");
        let answer = read_text(&mut client).unwrap().unwrap();
        assert_eq!(answer, "world");
        server.join().unwrap();
    }

    fn read_http_request_raw(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read(&mut byte).unwrap();
            buffer.push(byte[0]);
            if buffer.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(buffer).unwrap()
    }

    fn write_client_text(stream: &mut TcpStream, text: &str) {
        let payload = text.as_bytes();
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![0x81, 0x80 | payload.len() as u8];
        frame.extend(mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        stream.write_all(&frame).unwrap();
        stream.flush().unwrap();
    }
}
