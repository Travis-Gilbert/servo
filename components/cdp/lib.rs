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
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
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
    remember_listener(&shared, &external);
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

fn remember_listener(shared: &Shared, external: &Option<(TcpListener, u16, String)>) {
    if let Some((listener, port, token)) = external {
        shared
            .session
            .lock()
            .expect("cdp session")
            .set_listen(*port, token.clone());
        log::info!("CDP listening on 127.0.0.1:{port}");
        let _ = listener;
    }
}

fn external_listener() -> Option<(TcpListener, u16, String)> {
    if !pref!(cdp_server_enabled) {
        return None;
    }
    token_listener()
}

fn token_listener() -> Option<(TcpListener, u16, String)> {
    let token = pref!(cdp_server_token);
    if token.is_empty() {
        log::error!("CDP server not started: cdp_server_token is empty");
        return None;
    }
    bind_listener(token)
}

fn bind_listener(token: String) -> Option<(TcpListener, u16, String)> {
    let bound = TcpListener::bind(listen_address());
    listener_or_log(bound, token)
}

fn listener_or_log(
    bound: std::io::Result<TcpListener>,
    token: String,
) -> Option<(TcpListener, u16, String)> {
    if let Err(error) = &bound {
        log::error!("CDP listen failed: {error}");
        return None;
    }
    opened_listener(bound.unwrap(), token)
}

fn opened_listener(listener: TcpListener, token: String) -> Option<(TcpListener, u16, String)> {
    let Ok(address) = listener.local_addr() else {
        return None;
    };
    Some((listener, address.port(), token))
}

fn listen_address() -> SocketAddr {
    let configured = pref!(cdp_server_listen_address);
    if let Ok(address) = configured.parse() {
        return address;
    }
    port_address(&configured)
}

fn port_address(configured: &str) -> SocketAddr {
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
    let accepted = external.map(start_accept);
    while session_turn(&shared, &receiver, accepted.as_ref()) {}
}

fn start_accept((listener, token): (TcpListener, String)) -> Receiver<(TcpStream, String)> {
    let (accepted_sender, accepted_receiver) = unbounded();
    thread::Builder::new()
        .name("cdp-accept".to_owned())
        .spawn(move || accept_incoming(listener, token, accepted_sender))
        .expect("cdp accept thread");
    accepted_receiver
}

fn accept_incoming(listener: TcpListener, token: String, sender: Sender<(TcpStream, String)>) {
    let mut streams = listener.incoming();
    while take_stream(&mut streams, &sender, &token) {}
}

fn take_stream(
    streams: &mut std::net::Incoming<'_>,
    sender: &Sender<(TcpStream, String)>,
    token: &str,
) -> bool {
    if let Some(stream) = streams.next() {
        return forward_stream(stream, sender, token);
    }
    false
}

fn forward_stream(
    stream: std::io::Result<TcpStream>,
    sender: &Sender<(TcpStream, String)>,
    token: &str,
) -> bool {
    if let Ok(stream) = stream {
        return sender.send((stream, token.to_owned())).is_ok();
    }
    log::warn!("CDP accept failed");
    false
}

fn session_turn(
    shared: &Arc<Shared>,
    receiver: &Receiver<DevtoolsControlMsg>,
    accepted: Option<&Receiver<(TcpStream, String)>>,
) -> bool {
    if let Some(accepted) = accepted {
        return external_turn(shared, receiver, accepted);
    }
    script_turn(shared, receiver)
}

fn external_turn(
    shared: &Arc<Shared>,
    receiver: &Receiver<DevtoolsControlMsg>,
    accepted: &Receiver<(TcpStream, String)>,
) -> bool {
    crossbeam_channel::select! {
        recv(receiver) -> message => message_turn(shared, message),
        recv(accepted) -> accepted => accepted_turn(shared, accepted),
    }
}

fn message_turn(
    shared: &Shared,
    message: Result<DevtoolsControlMsg, crossbeam_channel::RecvError>,
) -> bool {
    if let Ok(message) = message {
        let events = shared.session.lock().expect("cdp session").control(message);
        deliver(shared, &events);
        return true;
    }
    false
}

fn accepted_turn(
    shared: &Arc<Shared>,
    accepted: Result<(TcpStream, String), crossbeam_channel::RecvError>,
) -> bool {
    if let Ok((stream, token)) = accepted {
        spawn_client(shared.clone(), stream, token);
        return true;
    }
    false
}

fn spawn_client(shared: Arc<Shared>, stream: TcpStream, token: String) {
    thread::Builder::new()
        .name("cdp-client".to_owned())
        .spawn(move || client(shared, stream, token))
        .expect("cdp client thread");
}

fn script_turn(shared: &Arc<Shared>, receiver: &Receiver<DevtoolsControlMsg>) -> bool {
    message_turn(shared, receiver.recv())
}

fn client(shared: Arc<Shared>, mut stream: TcpStream, token: String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60 * 30)));
    if let Ok(request) = websocket::read_http_request(&mut stream) {
        open_client(shared, stream, request, &token);
    }
}

fn open_client(
    shared: Arc<Shared>,
    mut stream: TcpStream,
    request: websocket::HttpRequest,
    token: &str,
) {
    if refuse_http(&shared, &mut stream, &request, token) {
        return;
    }
    upgrade_client(shared, stream, &request);
}

fn refuse_http(
    shared: &Shared,
    stream: &mut TcpStream,
    request: &websocket::HttpRequest,
    token: &str,
) -> bool {
    if !request_authorized(request, token) {
        let _ = websocket::write_http(stream, "401 Unauthorized", "text/plain", "");
        return true;
    }
    refuse_method(shared, stream, request)
}

fn refuse_method(
    shared: &Shared,
    stream: &mut TcpStream,
    request: &websocket::HttpRequest,
) -> bool {
    if request.method != "GET" {
        let _ = websocket::write_http(stream, "405 Method Not Allowed", "text/plain", "");
        return true;
    }
    answer_discovery(shared, stream, request)
}

fn answer_discovery(
    shared: &Shared,
    stream: &mut TcpStream,
    request: &websocket::HttpRequest,
) -> bool {
    let path = request.path.split('?').next().unwrap_or("/");
    if discovery_path(path) {
        let _ = websocket::write_http(
            stream,
            "200 OK",
            "application/json",
            &discovery_body(shared, path),
        );
        return true;
    }
    false
}

fn discovery_path(path: &str) -> bool {
    if path == "/json/version" {
        return true;
    }
    list_path(path)
}

fn list_path(path: &str) -> bool {
    path == "/json/list" || path == "/json"
}

fn discovery_body(shared: &Shared, path: &str) -> String {
    if path == "/json/version" {
        return version_body();
    }
    Value::Array(shared.session.lock().expect("cdp session").targets()).to_string()
}

fn version_body() -> String {
    json!({
        "Browser": "Platinum/Servo",
        "Protocol-Version": "1.3",
        "User-Agent": "Platinum",
        "V8-Version": "0",
        "WebKit-Version": "0",
    })
    .to_string()
}

fn upgrade_client(shared: Arc<Shared>, mut stream: TcpStream, request: &websocket::HttpRequest) {
    let path = request.path.split('?').next().unwrap_or("/");
    let Some(target) = path.strip_prefix("/devtools/page/") else {
        let _ = websocket::write_http(&mut stream, "404 Not Found", "text/plain", "");
        return;
    };
    accept_page(shared, stream, request, &websocket::decode_segment(target));
}

fn accept_page(
    shared: Arc<Shared>,
    mut stream: TcpStream,
    request: &websocket::HttpRequest,
    target: &str,
) {
    let Some(key) = websocket::header(request, "sec-websocket-key") else {
        let _ = websocket::write_http(&mut stream, "400 Bad Request", "text/plain", "");
        return;
    };
    if websocket::write_upgrade(&mut stream, &websocket::accept_key(key)).is_err() {
        return;
    }
    socket_session(shared, stream, target.to_owned());
}

fn socket_session(shared: Arc<Shared>, stream: TcpStream, target: String) {
    let _ = stream.set_nonblocking(true);
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let (outbound_tx, outbound_rx) = unbounded();
    let client_id = shared.session.lock().expect("cdp session").attach(target);
    shared
        .sinks
        .lock()
        .expect("cdp sinks")
        .insert(client_id, Sink::Socket(outbound_tx));
    while socket_turn(&shared, client_id, &mut socket, &outbound_rx) {}
    shared
        .session
        .lock()
        .expect("cdp session")
        .detach(client_id);
    shared.sinks.lock().expect("cdp sinks").remove(&client_id);
}

fn socket_turn(
    shared: &Shared,
    client_id: u64,
    socket: &mut WebSocket<TcpStream>,
    outbound: &Receiver<String>,
) -> bool {
    if write_waiting(socket, outbound) {
        return read_socket(shared, client_id, socket, outbound);
    }
    false
}

#[derive(PartialEq, Eq)]
enum Outbound {
    More,
    Idle,
    Closed,
}

fn write_waiting(socket: &mut WebSocket<TcpStream>, outbound: &Receiver<String>) -> bool {
    let mut step = Outbound::More;
    while step == Outbound::More {
        step = write_next(socket, outbound);
    }
    step == Outbound::Idle
}

fn write_next(socket: &mut WebSocket<TcpStream>, outbound: &Receiver<String>) -> Outbound {
    if let Ok(text) = outbound.try_recv() {
        return wrote_frame(socket, text);
    }
    Outbound::Idle
}

fn wrote_frame(socket: &mut WebSocket<TcpStream>, text: String) -> Outbound {
    if write_frame(socket, text) {
        return Outbound::More;
    }
    Outbound::Closed
}

fn write_frame(socket: &mut WebSocket<TcpStream>, text: String) -> bool {
    socket.send(Message::text(text)).is_ok() && socket.flush().is_ok()
}

fn read_socket(
    shared: &Shared,
    client_id: u64,
    socket: &mut WebSocket<TcpStream>,
    outbound: &Receiver<String>,
) -> bool {
    let read = socket.read();
    if let Ok(message) = read {
        return take_message(shared, client_id, socket, message);
    }
    idle_or_close(&read, outbound)
}

fn take_message(
    shared: &Shared,
    client_id: u64,
    socket: &mut WebSocket<TcpStream>,
    message: Message,
) -> bool {
    if let Message::Text(text) = message {
        return take_text(shared, client_id, &text);
    }
    take_control(socket, message)
}

fn take_text(shared: &Shared, client_id: u64, text: &str) -> bool {
    if let Ok(message) = serde_json::from_str::<Value>(text) {
        let outcome = shared
            .session
            .lock()
            .expect("cdp session")
            .command(client_id, &message);
        finish(shared, client_id, outcome);
    }
    true
}

fn take_control(socket: &mut WebSocket<TcpStream>, message: Message) -> bool {
    if let Message::Ping(payload) = message {
        return socket.send(Message::Pong(payload)).is_ok();
    }
    stay_open(message)
}

fn stay_open(message: Message) -> bool {
    if let Message::Close(_) = message {
        return false;
    }
    true
}

fn idle_or_close(read: &Result<Message, tungstenite::Error>, outbound: &Receiver<String>) -> bool {
    if let Err(tungstenite::Error::Io(error)) = read {
        return blocked(error, outbound);
    }
    false
}

fn blocked(error: &std::io::Error, outbound: &Receiver<String>) -> bool {
    if error.kind() == ErrorKind::WouldBlock {
        pause_when_idle(outbound);
        return true;
    }
    false
}

fn pause_when_idle(outbound: &Receiver<String>) {
    if outbound.is_empty() {
        thread::sleep(Duration::from_millis(10));
    }
}

fn finish(shared: &Shared, client_id: u64, outcome: CommandOutcome) {
    if let CommandOutcome::Messages(messages) = outcome {
        deliver_one(shared, client_id, &messages);
        return;
    }
    finish_pending(shared, client_id, outcome);
}

fn finish_pending(shared: &Shared, client_id: u64, outcome: CommandOutcome) {
    let CommandOutcome::Pending {
        id,
        sender,
        request,
    } = outcome
    else {
        return;
    };
    let parent = request_parent(&request);
    let invocation = request_invocation(&request);
    let reply = classify_reply(dispatch(&sender, request), parent, invocation);
    let messages = shared
        .session
        .lock()
        .expect("cdp session")
        .complete(id, client_id, reply);
    deliver_one(shared, client_id, &messages);
}

fn request_parent(request: &ScriptRequest) -> Option<i64> {
    if let ScriptRequest::Children { parent, .. } = request {
        return Some(*parent);
    }
    None
}

fn request_invocation(request: &ScriptRequest) -> Option<String> {
    if let ScriptRequest::InvokeTool { invocation, .. } = request {
        return Some(invocation.clone());
    }
    None
}

fn classify_reply(
    reply: ScriptReply,
    parent: Option<i64>,
    invocation: Option<String>,
) -> ScriptReply {
    if let ScriptReply::Children(nodes) = reply {
        return children_of(nodes, parent);
    }
    empty_tool(reply, invocation)
}

fn children_of(nodes: Option<Vec<devtools_traits::NodeInfo>>, parent: Option<i64>) -> ScriptReply {
    if let Some(parent) = parent {
        return ScriptReply::ChildrenOf { parent, nodes };
    }
    ScriptReply::Children(nodes)
}

fn empty_tool(reply: ScriptReply, invocation: Option<String>) -> ScriptReply {
    if let ScriptReply::Empty = reply {
        return blank_tool(invocation);
    }
    reply
}

fn blank_tool(invocation: Option<String>) -> ScriptReply {
    if let Some(invocation) = invocation {
        return ScriptReply::Tool(
            invocation,
            WebMcpInvokeResult {
                ok: true,
                result: String::new(),
                error: None,
            },
        );
    }
    ScriptReply::Empty
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
    if let Some(sink) = sink {
        send_one(sink, message);
    }
}

fn send_one(sink: &Sink, message: &Value) {
    if let Sink::Socket(sender) = sink {
        let _ = sender.send(message.to_string());
        return;
    }
    send_builtin(sink, message);
}

fn send_builtin(sink: &Sink, message: &Value) {
    if let Sink::Builtin(sender) = sink {
        let _ = sender.send(message.clone());
    }
}

fn request_authorized(request: &websocket::HttpRequest, token: &str) -> bool {
    let host = websocket::header(request, "host").unwrap_or("");
    if !host_is_loopback(host) {
        return false;
    }
    origin_and_token(request, token)
}

fn origin_and_token(request: &websocket::HttpRequest, token: &str) -> bool {
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
    if let ScriptRequest::Eval {
        pipeline,
        expression,
    } = request
    {
        return ask(
            sender,
            |reply| DevtoolScriptControlMsg::Eval(expression, pipeline, None, false, reply),
            ScriptReply::Eval,
        );
    }
    dispatch_root(sender, request)
}

fn dispatch_root(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::Root { pipeline } = request {
        return ask(
            sender,
            |reply| DevtoolScriptControlMsg::GetRootNode(pipeline, reply),
            ScriptReply::Root,
        );
    }
    dispatch_children(sender, request)
}

fn dispatch_children(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::Children { pipeline, node, .. } = request {
        return ask(
            sender,
            |reply| DevtoolScriptControlMsg::GetChildren(pipeline, node, reply),
            ScriptReply::Children,
        );
    }
    dispatch_style(sender, request)
}

fn dispatch_style(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::ComputedStyle { pipeline, node } = request {
        return ask(
            sender,
            |reply| DevtoolScriptControlMsg::GetComputedStyle(pipeline, node, reply),
            ScriptReply::Computed,
        );
    }
    dispatch_attribute(sender, request)
}

fn dispatch_attribute(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::Attribute {
        pipeline,
        node,
        name,
        value,
    } = request
    {
        return tell(
            sender,
            DevtoolScriptControlMsg::ModifyAttribute(
                pipeline,
                node,
                vec![devtools_traits::AttrModification {
                    attribute_name: name,
                    new_value: Some(value),
                }],
            ),
        );
    }
    dispatch_highlight(sender, request)
}

fn dispatch_highlight(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::Highlight { pipeline, node } = request {
        return tell(
            sender,
            DevtoolScriptControlMsg::HighlightDomNode(pipeline, node),
        );
    }
    dispatch_tool(sender, request)
}

fn dispatch_tool(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::InvokeTool {
        pipeline,
        invocation,
        name,
        arguments,
    } = request
    {
        let id = invocation.clone();
        return ask(
            sender,
            |reply| DevtoolScriptControlMsg::WebMcpInvoke(pipeline, id, name, arguments, reply),
            |result| ScriptReply::Tool(invocation, result),
        );
    }
    dispatch_cancel(sender, request)
}

fn dispatch_cancel(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    request: ScriptRequest,
) -> ScriptReply {
    if let ScriptRequest::CancelTool {
        pipeline,
        invocation,
    } = request
    {
        return tell(
            sender,
            DevtoolScriptControlMsg::WebMcpCancel(pipeline, invocation),
        );
    }
    ScriptReply::Missing
}

fn ask<T, Build, Wrap>(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    build: Build,
    wrap: Wrap,
) -> ScriptReply
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    Build: FnOnce(GenericSender<T>) -> DevtoolScriptControlMsg,
    Wrap: FnOnce(T) -> ScriptReply,
{
    let Some((reply_sender, reply_receiver)) = generic_channel::channel() else {
        return ScriptReply::Missing;
    };
    send_for_reply(sender, build(reply_sender), reply_receiver, wrap)
}

fn send_for_reply<T, Wrap>(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    message: DevtoolScriptControlMsg,
    reply_receiver: GenericReceiver<T>,
    wrap: Wrap,
) -> ScriptReply
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    Wrap: FnOnce(T) -> ScriptReply,
{
    if sender.send(message).is_err() {
        return ScriptReply::Missing;
    }
    take_reply(reply_receiver, wrap)
}

fn take_reply<T, Wrap>(reply_receiver: GenericReceiver<T>, wrap: Wrap) -> ScriptReply
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    Wrap: FnOnce(T) -> ScriptReply,
{
    if let Ok(value) = reply_receiver.recv() {
        return wrap(value);
    }
    ScriptReply::Missing
}

fn tell(
    sender: &GenericSender<DevtoolScriptControlMsg>,
    message: DevtoolScriptControlMsg,
) -> ScriptReply {
    if sender.send(message).is_err() {
        return ScriptReply::Missing;
    }
    ScriptReply::Empty
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
