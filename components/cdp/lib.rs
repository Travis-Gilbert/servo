/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CDP endpoint. Platinum's prototype connects here. The Chrome DevTools
//! frontend is not part of this server.

#![crate_name = "cdp"]
#![crate_type = "rlib"]

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use devtools_traits::{
    DevtoolScriptControlMsg, DevtoolsControlMsg, EvaluateJSReply, WebMcpInvokeResult,
};
use serde_json::{Value, json};
use servo_base::generic_channel::{self, GenericSender};
use servo_base::id::PipelineId;
use servo_config::pref;

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
    /// Cloned write halves, one per attached client.
    writers: Mutex<Vec<(u64, TcpStream)>>,
}

/// Listen on the CDP preference and return the sender script and net already use.
/// The Firefox devtools server is left alone; the caller picks one consumer.
pub fn start_server() -> Option<Sender<DevtoolsControlMsg>> {
    let address = listen_address();
    let listener = TcpListener::bind(address).ok()?;
    let port = listener.local_addr().ok()?.port();
    log::info!("CDP listening on 127.0.0.1:{port}");
    let (sender, receiver) = unbounded();
    thread::Builder::new()
        .name("cdp".to_owned())
        .spawn(move || run(listener, receiver))
        .ok()?;
    Some(sender)
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

fn run(listener: TcpListener, receiver: Receiver<DevtoolsControlMsg>) {
    let shared = Arc::new(Shared {
        session: Mutex::new(Session::new()),
        writers: Mutex::new(Vec::new()),
    });
    let (accepted_sender, accepted_receiver) = unbounded();
    thread::Builder::new()
        .name("cdp-accept".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if accepted_sender.send(stream).is_err() {
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

    loop {
        crossbeam_channel::select! {
            recv(receiver) -> message => {
                let Ok(message) = message else { break };
                let events = shared.session.lock().expect("cdp session").control(message);
                deliver(&shared, &events);
            },
            recv(accepted_receiver) -> stream => {
                let Ok(stream) = stream else { break };
                let shared = shared.clone();
                thread::Builder::new()
                    .name("cdp-client".to_owned())
                    .spawn(move || client(shared, stream))
                    .expect("cdp client thread");
            },
        }
    }
}

fn client(shared: Arc<Shared>, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60 * 30)));
    let request = match websocket::read_http_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            log::debug!("CDP http read failed: {error}");
            return;
        },
    };
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
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let client_id = shared.session.lock().expect("cdp session").attach(target);
    shared
        .writers
        .lock()
        .expect("cdp writers")
        .push((client_id, writer));
    loop {
        let text = match websocket::read_text(&mut stream) {
            Ok(Some(text)) => text,
            Ok(None) | Err(_) => break,
        };
        let message: Value = match serde_json::from_str(&text) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let outcome = shared
            .session
            .lock()
            .expect("cdp session")
            .command(client_id, &message);
        match outcome {
            CommandOutcome::Messages(messages) => {
                for message in messages {
                    let _ = websocket::write_text(&mut stream, &message.to_string());
                }
            },
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
                for message in messages {
                    let _ = websocket::write_text(&mut stream, &message.to_string());
                }
            },
        }
    }
    shared
        .session
        .lock()
        .expect("cdp session")
        .detach(client_id);
    shared
        .writers
        .lock()
        .expect("cdp writers")
        .retain(|(id, _)| *id != client_id);
}

fn deliver(shared: &Shared, events: &[(u64, Value)]) {
    let mut writers = shared.writers.lock().expect("cdp writers");
    for (client, message) in events {
        if let Some((_, stream)) = writers.iter_mut().find(|(id, _)| id == client) {
            let _ = websocket::write_text(stream, &message.to_string());
        }
    }
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

#[allow(dead_code)]
fn _reply_type(reply: EvaluateJSReply) -> EvaluateJSReply {
    reply
}

#[allow(dead_code)]
fn _write_marker(stream: &mut TcpStream) {
    let _ = stream.write_all(&[]);
}
