/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CDP endpoint. Platinum's prototype connects here. The Chrome DevTools
//! frontend is not part of this server.

#![crate_name = "cdp"]
#![crate_type = "rlib"]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use devtools_traits::{DevtoolScriptControlMsg, DevtoolsControlMsg, WebMcpInvokeResult};
use serde_json::{Value, json};
use servo_base::generic_channel::{self, GenericSender};
use servo_base::id::PipelineId;
use servo_config::pref;
use tungstenite::Message;
use tungstenite::protocol::{Role, WebSocket};

mod session;
mod websocket;

use session::{CommandOutcome, ScriptReply, Session};

/// A request the CDP thread asks the script thread to answer.
pub(crate) enum ScriptRequest {
    Eval {
        pipeline: PipelineId,
        expression: String,
    },
    Root {
        pipeline: PipelineId,
    },
    Children {
        pipeline: PipelineId,
        parent: i64,
        node: String,
    },
    Attribute {
        pipeline: PipelineId,
        node: String,
        name: String,
        value: String,
    },
    ComputedStyle {
        pipeline: PipelineId,
        node: String,
    },
    Highlight {
        pipeline: PipelineId,
        node: Option<String>,
    },
    InvokeTool {
        pipeline: PipelineId,
        invocation: String,
        name: String,
        arguments: String,
    },
    CancelTool {
        pipeline: PipelineId,
        invocation: String,
    },
}

struct Shared {
    session: Mutex<Session>,
    /// One outbound queue per client. The socket owner is the only writer.
    sinks: Mutex<HashMap<u64, Sink>>,
}

enum Sink {
    Socket(Sender<String>),
    Builtin(Sender<Value>),
}

/// In-process CDP client. It does not open a socket.
pub struct BuiltinClient {
    shared: Arc<Shared>,
    events_tx: Sender<Value>,
    events: Receiver<Value>,
    client: Mutex<Option<u64>>,
}

impl BuiltinClient {
    /// Attach to the webview whose id is `target`.
    pub fn attach(&self, target: &str) {
        self.detach();
        let id = self
            .shared
            .session
            .lock()
            .expect("cdp session")
            .attach(target.to_owned());
        self.shared
            .sinks
            .lock()
            .expect("cdp sinks")
            .insert(id, Sink::Builtin(self.events_tx.clone()));
        *self.client.lock().expect("cdp client") = Some(id);
    }

    /// Drop the attachment so later events are not delivered here.
    pub fn detach(&self) {
        let Some(id) = self.client.lock().expect("cdp client").take() else {
            return;
        };
        self.shared.session.lock().expect("cdp session").detach(id);
        self.shared.sinks.lock().expect("cdp sinks").remove(&id);
    }

    /// Send one CDP command to the attached target.
    pub fn send(&self, message: &Value) {
        let Some(id) = *self.client.lock().expect("cdp client") else {
            return;
        };
        let outcome = self
            .shared
            .session
            .lock()
            .expect("cdp session")
            .command(id, message);
        finish(&self.shared, id, outcome);
    }

    pub fn try_recv(&self) -> Result<Value, TryRecvError> {
        self.events.try_recv()
    }
}

static BUILTIN: Mutex<Option<BuiltinClient>> = Mutex::new(None);

/// Take the embedder's in-process client. One Servo process has one.
pub fn take_builtin_client() -> Option<BuiltinClient> {
    BUILTIN.lock().expect("cdp builtin").take()
}

/// Start the inspection session. A TCP listener is opened only when the
/// external server preference is on and a token is set.
pub fn start_server() -> Option<Sender<DevtoolsControlMsg>> {
    let external = external_listener();
    let (sender, receiver) = unbounded();
    let (events_tx, events_rx) = unbounded();
    let shared = Arc::new(Shared {
        session: Mutex::new(Session::new()),
        sinks: Mutex::new(HashMap::new()),
    });
    if let Some((listener, port, token)) = &external {
        shared
            .session
            .lock()
            .expect("cdp session")
            .set_listen(*port, token.clone());
        log::info!("CDP listening on 127.0.0.1:{port}");
        let _ = listener;
    }
    let client = BuiltinClient {
        shared: shared.clone(),
        events_tx,
        events: events_rx,
        client: Mutex::new(None),
    };
    *BUILTIN.lock().expect("cdp builtin") = Some(client);
    let accept = external.map(|(listener, _, token)| (listener, token));
    thread::Builder::new()
        .name("cdp".to_owned())
        .spawn(move || run(shared, receiver, accept))
        .ok()?;
    Some(sender)
}

fn external_listener() -> Option<(TcpListener, u16, String)> {
    if !pref!(cdp_server_enabled) {
        return None;
    }
    let token = pref!(cdp_server_token);
    if token.is_empty() {
        log::error!("CDP server not started: cdp_server_token is empty");
        return None;
    }
    let address = listen_address();
    match TcpListener::bind(address) {
        Ok(listener) => {
            let port = listener.local_addr().ok()?.port();
            Some((listener, port, token))
        },
        Err(error) => {
            log::error!("CDP listen failed: {error}");
            None
        },
    }
}

fn listen_address() -> SocketAddr {
    let configured = pref!(cdp_server_listen_address);
    if let Ok(address) = configured.parse() {
        return address;
    }
    if let Ok(port) = configured.parse::<u16>() {
        return SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    }
    SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9333)
}

fn run(
    shared: Arc<Shared>,
    receiver: Receiver<DevtoolsControlMsg>,
    external: Option<(TcpListener, String)>,
) {
    let accepted = external.map(|(listener, token)| {
        let (accepted_sender, accepted_receiver) = unbounded();
        thread::Builder::new()
            .name("cdp-accept".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            if accepted_sender.send((stream, token.clone())).is_err() {
                                break;
                            }
                        },
                        Err(error) => {
                            log::warn!("CDP accept failed: {error}");
                            break;
                        },
                    }
                }
            })
            .expect("cdp accept thread");
        accepted_receiver
    });

    loop {
        if let Some(accepted) = &accepted {
            crossbeam_channel::select! {
                recv(receiver) -> message => {
                    let Ok(message) = message else { break };
                    let events = shared.session.lock().expect("cdp session").control(message);
                    deliver(&shared, &events);
                },
                recv(accepted) -> accepted => {
                    let Ok((stream, token)) = accepted else { break };
                    let shared = shared.clone();
                    thread::Builder::new()
                        .name("cdp-client".to_owned())
                        .spawn(move || client(shared, stream, token))
                        .expect("cdp client thread");
                },
            }
        } else {
            let Ok(message) = receiver.recv() else { break };
            let events = shared.session.lock().expect("cdp session").control(message);
            deliver(&shared, &events);
        }
    }
}

fn client(shared: Arc<Shared>, mut stream: TcpStream, token: String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60 * 30)));
    let request = match websocket::read_http_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            log::debug!("CDP http read failed: {error}");
            return;
        },
    };
    if !request_authorized(&request, &token) {
        let _ = websocket::write_http(&mut stream, "401 Unauthorized", "text/plain", "");
        return;
    }
    if request.method != "GET" {
        let _ = websocket::write_http(&mut stream, "405 Method Not Allowed", "text/plain", "");
        return;
    }
    let path = request.path.split('?').next().unwrap_or("/");
    if path == "/json/version" || path == "/json/list" || path == "/json" {
        let body = if path == "/json/version" {
            json!({
                "Browser": "Platinum/Servo",
                "Protocol-Version": "1.3",
                "User-Agent": "Platinum",
                "V8-Version": "0",
                "WebKit-Version": "0",
            })
            .to_string()
        } else {
            let targets = shared.session.lock().expect("cdp session").targets();
            Value::Array(targets).to_string()
        };
        let _ = websocket::write_http(&mut stream, "200 OK", "application/json", &body);
        return;
    }
    let Some(target) = path.strip_prefix("/devtools/page/") else {
        let _ = websocket::write_http(&mut stream, "404 Not Found", "text/plain", "");
        return;
    };
    let target = websocket::decode_segment(target);
    let Some(key) = websocket::header(&request, "sec-websocket-key") else {
        let _ = websocket::write_http(&mut stream, "400 Bad Request", "text/plain", "");
        return;
    };
    if websocket::write_upgrade(&mut stream, &websocket::accept_key(key)).is_err() {
        return;
    }
    let _ = stream.set_nonblocking(true);
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let (outbound_tx, outbound_rx) = unbounded();
    let client_id = shared.session.lock().expect("cdp session").attach(target);
    shared
        .sinks
        .lock()
        .expect("cdp sinks")
        .insert(client_id, Sink::Socket(outbound_tx));
    'connection: loop {
        while let Ok(text) = outbound_rx.try_recv() {
            if socket.send(Message::text(text)).is_err() || socket.flush().is_err() {
                break 'connection;
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(message) = serde_json::from_str::<Value>(text.as_ref()) {
                    let outcome = shared
                        .session
                        .lock()
                        .expect("cdp session")
                        .command(client_id, &message);
                    finish(&shared, client_id, outcome);
                }
            },
            Ok(Message::Ping(payload)) => {
                if socket.send(Message::Pong(payload)).is_err() {
                    break;
                }
            },
            Ok(Message::Close(_)) => break,
            Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                if outbound_rx.is_empty() {
                    thread::sleep(Duration::from_millis(10));
                }
            },
            Err(_) => break,
            _ => {},
        }
    }
    shared
        .session
        .lock()
        .expect("cdp session")
        .detach(client_id);
    shared.sinks.lock().expect("cdp sinks").remove(&client_id);
}

fn finish(shared: &Shared, client_id: u64, outcome: CommandOutcome) {
    match outcome {
        CommandOutcome::Messages(messages) => deliver_one(shared, client_id, &messages),
        CommandOutcome::Pending {
            id,
            sender,
            request,
        } => {
            let parent = match &request {
                ScriptRequest::Children { parent, .. } => Some(*parent),
                _ => None,
            };
            let invocation = match &request {
                ScriptRequest::InvokeTool { invocation, .. } => Some(invocation.clone()),
                _ => None,
            };
            let reply = dispatch(&sender, request);
            let reply = match (reply, parent, invocation) {
                (ScriptReply::Children(nodes), Some(parent), _) => {
                    ScriptReply::ChildrenOf { parent, nodes }
                },
                (ScriptReply::Tool(invocation, result), _, _) => {
                    ScriptReply::Tool(invocation, result)
                },
                (other, _, Some(invocation)) if matches!(other, ScriptReply::Empty) => {
                    ScriptReply::Tool(
                        invocation,
                        WebMcpInvokeResult {
                            ok: true,
                            result: String::new(),
                            error: None,
                        },
                    )
                },
                (other, _, _) => other,
            };
            let messages = shared
                .session
                .lock()
                .expect("cdp session")
                .complete(id, client_id, reply);
            deliver_one(shared, client_id, &messages);
        },
    }
}

fn deliver(shared: &Shared, events: &[(u64, Value)]) {
    let sinks = shared.sinks.lock().expect("cdp sinks");
    for (client, message) in events {
        send_sink(sinks.get(client), message);
    }
}

fn deliver_one(shared: &Shared, client: u64, messages: &[Value]) {
    let sinks = shared.sinks.lock().expect("cdp sinks");
    let sink = sinks.get(&client);
    for message in messages {
        send_sink(sink, message);
    }
}

fn send_sink(sink: Option<&Sink>, message: &Value) {
    match sink {
        Some(Sink::Socket(sender)) => {
            let _ = sender.send(message.to_string());
        },
        Some(Sink::Builtin(sender)) => {
            let _ = sender.send(message.clone());
        },
        None => {},
    }
}

fn request_authorized(request: &websocket::HttpRequest, token: &str) -> bool {
    let host = websocket::header(request, "host").unwrap_or("");
    if !host_is_loopback(host) {
        return false;
    }
    if !origin_allowed(websocket::header(request, "origin")) {
        return false;
    }
    query_token(&request.path) == Some(token)
}

fn host_is_loopback(value: &str) -> bool {
    let value = value.trim();
    let host = if let Some(rest) = value.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        value.split(':').next().unwrap_or(value)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn origin_allowed(origin: Option<&str>) -> bool {
    match origin.map(str::trim) {
        None | Some("") | Some("null") => true,
        Some(origin) => {
            origin.starts_with("devtools://") || origin.starts_with("chrome-devtools://")
        },
    }
}

fn query_token(path: &str) -> Option<&str> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "token").then_some(value)
    })
}

fn dispatch(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    match request {
        ScriptRequest::Eval {
            pipeline,
            expression,
        } => {
            let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
                return ScriptReply::Missing;
            };
            if sender
                .send(DevtoolScriptControlMsg::Eval(
                    expression,
                    pipeline,
                    None,
                    false,
                    reply_sender,
                ))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            match reply_receiver.recv() {
                Ok(reply) => ScriptReply::Eval(reply),
                Err(_) => ScriptReply::Missing,
            }
        },
        ScriptRequest::Root { pipeline } => {
            let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
                return ScriptReply::Missing;
            };
            if sender
                .send(DevtoolScriptControlMsg::GetRootNode(pipeline, reply_sender))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            match reply_receiver.recv() {
                Ok(node) => ScriptReply::Root(node),
                Err(_) => ScriptReply::Missing,
            }
        },
        ScriptRequest::Children { pipeline, node, .. } => {
            let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
                return ScriptReply::Missing;
            };
            if sender
                .send(DevtoolScriptControlMsg::GetChildren(
                    pipeline,
                    node,
                    reply_sender,
                ))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            match reply_receiver.recv() {
                Ok(nodes) => ScriptReply::Children(nodes),
                Err(_) => ScriptReply::Missing,
            }
        },
        ScriptRequest::ComputedStyle { pipeline, node } => {
            let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
                return ScriptReply::Missing;
            };
            if sender
                .send(DevtoolScriptControlMsg::GetComputedStyle(
                    pipeline,
                    node,
                    reply_sender,
                ))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            match reply_receiver.recv() {
                Ok(styles) => ScriptReply::Computed(styles),
                Err(_) => ScriptReply::Missing,
            }
        },
        ScriptRequest::Attribute {
            pipeline,
            node,
            name,
            value,
        } => {
            let modification = devtools_traits::AttrModification {
                attribute_name: name,
                new_value: Some(value),
            };
            if sender
                .send(DevtoolScriptControlMsg::ModifyAttribute(
                    pipeline,
                    node,
                    vec![modification],
                ))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            ScriptReply::Empty
        },
        ScriptRequest::Highlight { pipeline, node } => {
            if sender
                .send(DevtoolScriptControlMsg::HighlightDomNode(pipeline, node))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            ScriptReply::Empty
        },
        ScriptRequest::InvokeTool {
            pipeline,
            invocation,
            name,
            arguments,
        } => {
            let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
                return ScriptReply::Missing;
            };
            if sender
                .send(DevtoolScriptControlMsg::WebMcpInvoke(
                    pipeline,
                    invocation.clone(),
                    name,
                    arguments,
                    reply_sender,
                ))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            match reply_receiver.recv() {
                Ok(result) => ScriptReply::Tool(invocation, result),
                Err(_) => ScriptReply::Missing,
            }
        },
        ScriptRequest::CancelTool {
            pipeline,
            invocation,
        } => {
            if sender
                .send(DevtoolScriptControlMsg::WebMcpCancel(pipeline, invocation))
                .is_err()
            {
                return ScriptReply::Missing;
            }
            ScriptReply::Empty
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{host_is_loopback, origin_allowed, query_token};

    #[test]
    fn loopback_hosts_and_web_origins() {
        assert!(host_is_loopback("127.0.0.1:9333"));
        assert!(host_is_loopback("[::1]:9333"));
        assert!(!host_is_loopback("evil.example:9333"));
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("devtools://devtools")));
        assert!(!origin_allowed(Some("https://evil.example")));
        assert_eq!(query_token("/devtools/page/1?token=secret"), Some("secret"));
    }
}
