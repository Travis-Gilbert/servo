/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CDP session state for one Servo process.
//!
//! A target id is the `WebViewId` display string, which survives navigation.
//! An execution context id belongs to one `PipelineId` and is replaced when
//! that document is replaced. The methods implemented here are the ones the
//! Platinum prototype sends. Anything else is a method-not-found error.

use std::collections::{HashMap, VecDeque};
use std::time::UNIX_EPOCH;

use devtools_traits::{
    ChromeToDevtoolsControlMsg, ConsoleMessage, DebuggerValue, DevtoolsControlMsg,
    DevtoolsPageInfo, EvaluateJSReply, NavigationState, NetworkEvent, NodeInfo, NodeStyle,
    PageError, ScriptToDevtoolsControlMsg, WebMcpToolRecord,
};
use embedder_traits::ConsoleLogLevel;
use http::HeaderMap;
use net_traits::request::Destination;
use serde_json::{Value, json};
use servo_base::generic_channel::GenericSender;
use servo_base::id::{PipelineId, WebViewId};
use servo_url::ServoUrl;

use crate::ScriptRequest;

const CONSOLE_BUFFER: usize = 200;
const NETWORK_BODY_CAP: usize = 1024 * 1024;
const MAX_REQUESTS: usize = 100;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Domains {
    runtime: bool,
    log: bool,
    page: bool,
    dom: bool,
    network: bool,
    webmcp: bool,
}

impl Default for Domains {
    fn default() -> Self {
        Self {
            runtime: false,
            log: false,
            page: false,
            dom: false,
            network: false,
            webmcp: false,
        }
    }
}

struct Target {
    webview: WebViewId,
    browsing_context: String,
    pipeline: Option<PipelineId>,
    context_id: Option<i64>,
    sender: Option<GenericSender<devtools_traits::DevtoolScriptControlMsg>>,
    title: String,
    url: String,
    origin: String,
    top_level: bool,
    console: VecDeque<Value>,
    nodes: HashMap<i64, String>,
    node_ids: HashMap<String, i64>,
    next_node: i64,
    requests: HashMap<String, NetRequest>,
    request_order: VecDeque<String>,
    tools: Vec<WebMcpToolRecord>,
}

struct NetRequest {
    events_sent: bool,
    response_sent: bool,
    finished: bool,
    body: Option<Vec<u8>>,
}

struct Client {
    target: String,
    domains: Domains,
}

pub(crate) struct Session {
    targets: HashMap<String, Target>,
    pipelines: HashMap<PipelineId, String>,
    clients: HashMap<u64, Client>,
    next_client: u64,
    next_context: i64,
    /// Invocation ids whose cancel arrived before the script reply.
    cancelled: HashMap<String, bool>,
    /// External listener, when one is bound. Discovery URLs use this port.
    listen_port: u16,
    token: String,
}

pub(crate) enum CommandOutcome {
    Messages(Vec<Value>),
    Pending {
        id: i64,
        sender: GenericSender<devtools_traits::DevtoolScriptControlMsg>,
        request: ScriptRequest,
    },
}

impl Session {
    pub(crate) fn new() -> Self {
        Self {
            targets: HashMap::new(),
            pipelines: HashMap::new(),
            clients: HashMap::new(),
            next_client: 1,
            next_context: 1,
            cancelled: HashMap::new(),
            listen_port: 0,
            token: String::new(),
        }
    }

    pub(crate) fn set_listen(&mut self, port: u16, token: String) {
        self.listen_port = port;
        self.token = token;
    }

    pub(crate) fn targets(&self) -> Vec<Value> {
        self.targets
            .values()
            .map(|target| {
                json!({
                    "id": target_key(target.webview),
                    "title": target.title,
                    "url": target.url,
                    "type": "page",
                    "webSocketDebuggerUrl": format!(
                        "ws://127.0.0.1:{}/devtools/page/{}?token={}",
                        self.listen_port,
                        crate::websocket::encode_segment(&target_key(target.webview)),
                        self.token
                    ),
                })
            })
            .collect()
    }

    pub(crate) fn attach(&mut self, target: String) -> u64 {
        let id = self.next_client;
        self.next_client += 1;
        self.clients.insert(
            id,
            Client {
                target,
                domains: Domains::default(),
            },
        );
        id
    }

    pub(crate) fn detach(&mut self, client: u64) {
        let target = self
            .clients
            .get(&client)
            .map(|client| client.target.clone());
        self.clients.remove(&client);
        let Some(key) = target else {
            return;
        };
        if self.network_open(&key) {
            return;
        }
        if let Some(target) = self.targets.get_mut(&key) {
            target.requests.clear();
            target.request_order.clear();
        }
    }

    pub(crate) fn control(&mut self, message: DevtoolsControlMsg) -> Vec<(u64, Value)> {
        match message {
            DevtoolsControlMsg::FromScript(message) => self.script(message),
            DevtoolsControlMsg::FromChrome(ChromeToDevtoolsControlMsg::NetworkEvent(id, event)) => {
                self.network(id, event)
            },
            DevtoolsControlMsg::FromChrome(_) | DevtoolsControlMsg::ClientExited => Vec::new(),
        }
    }

    pub(crate) fn command(&mut self, client: u64, message: &Value) -> CommandOutcome {
        let id = message.get("id").and_then(Value::as_i64);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(target_key) = self
            .clients
            .get(&client)
            .map(|client| client.target.clone())
        else {
            return CommandOutcome::Messages(error(id, -32600, "no session"));
        };
        if method_enables(method) {
            self.enable(client, method);
            let mut messages = success(id, json!({}));
            messages.extend(self.replay(client));
            return CommandOutcome::Messages(messages);
        }
        match method {
            "Runtime.evaluate" => self.evaluate(id, &target_key, &params),
            "Page.getFrameTree" => CommandOutcome::Messages(success(
                id,
                json!({ "frameTree": self.frame(&target_key) }),
            )),
            "DOM.getDocument" => {
                self.pending(id, &target_key, |pipeline| ScriptRequest::Root { pipeline })
            },
            "DOM.requestChildNodes" => {
                let node = params.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
                let unique = self_node(self, &target_key, node);
                self.pending(id, &target_key, move |pipeline| ScriptRequest::Children {
                    pipeline,
                    parent: node,
                    node: unique,
                })
            },
            "DOM.setAttributeValue" => {
                let node = params.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let unique = self_node(self, &target_key, node);
                self.pending(id, &target_key, move |pipeline| ScriptRequest::Attribute {
                    pipeline,
                    node: unique,
                    name,
                    value,
                })
            },
            "CSS.getComputedStyleForNode" => {
                let node = params.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
                let unique = self_node(self, &target_key, node);
                self.pending(id, &target_key, move |pipeline| {
                    ScriptRequest::ComputedStyle {
                        pipeline,
                        node: unique,
                    }
                })
            },
            "Overlay.highlightNode" => {
                let node_id = params.get("nodeId").and_then(Value::as_i64);
                let unique = node_id.map(|node| self_node(self, &target_key, node));
                self.pending(id, &target_key, move |pipeline| ScriptRequest::Highlight {
                    pipeline,
                    node: unique,
                })
            },
            "Overlay.hideHighlight" => {
                self.pending(id, &target_key, |pipeline| ScriptRequest::Highlight {
                    pipeline,
                    node: None,
                })
            },
            "Network.getResponseBody" => {
                let request_id = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                CommandOutcome::Messages(success(id, self.response_body(&target_key, &request_id)))
            },
            "WebMCP.invokeTool" => self.invoke_tool(id, &target_key, &params),
            "WebMCP.cancelInvocation" => {
                let invocation = params
                    .get("invocationId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.cancelled.insert(invocation.clone(), true);
                self.pending(id, &target_key, |pipeline| ScriptRequest::CancelTool {
                    pipeline,
                    invocation,
                })
            },
            _ => CommandOutcome::Messages(error(id, -32601, "Method not found")),
        }
    }

    pub(crate) fn complete(&mut self, id: i64, client: u64, reply: ScriptReply) -> Vec<Value> {
        let Some(target_key) = self
            .clients
            .get(&client)
            .map(|client| client.target.clone())
        else {
            return error(Some(id), -32600, "no session");
        };
        match reply {
            ScriptReply::Eval(reply) => success(Some(id), eval_result(&reply)),
            ScriptReply::Children(nodes) => {
                self.complete(id, client, ScriptReply::ChildrenOf { parent: 0, nodes })
            },
            ScriptReply::Root(node) => {
                let root = node.map(|node| self.adopt_node(&target_key, &node, true));
                success(Some(id), json!({ "root": root.unwrap_or(Value::Null) }))
            },
            ScriptReply::ChildrenOf { parent, nodes } => {
                let children: Vec<Value> = nodes
                    .unwrap_or_default()
                    .iter()
                    .map(|node| self.adopt_node(&target_key, node, false))
                    .collect();
                let mut messages = success(Some(id), json!({}));
                messages.push(event(
                    "DOM.setChildNodes",
                    json!({ "parentId": parent, "nodes": children }),
                ));
                messages
            },
            ScriptReply::Computed(styles) => {
                let computed: Vec<Value> = styles
                    .unwrap_or_default()
                    .iter()
                    .map(|style| json!({ "name": style.name, "value": style.value }))
                    .collect();
                success(Some(id), json!({ "computedStyle": computed }))
            },
            ScriptReply::Empty => success(Some(id), json!({})),
            ScriptReply::Tool(invocation, result) => {
                let cancelled = self.cancelled.remove(&invocation).unwrap_or(false);
                let status = if cancelled {
                    "cancelled"
                } else if result.ok {
                    "success"
                } else {
                    "error"
                };
                let output = serde_json::from_str::<Value>(&result.result)
                    .unwrap_or_else(|_| Value::String(result.result.clone()));
                let mut messages = success(Some(id), json!({ "invocationId": invocation }));
                messages.push(event(
                    "WebMCP.toolResponded",
                    json!({
                        "invocationId": invocation,
                        "status": status,
                        "output": output,
                        "errorText": result.error,
                    }),
                ));
                messages
            },
            ScriptReply::Missing => error(Some(id), -32000, "inspection unavailable"),
        }
    }

    fn enable(&mut self, client: u64, method: &str) {
        let Some(client) = self.clients.get_mut(&client) else {
            return;
        };
        match method {
            "Runtime.enable" => client.domains.runtime = true,
            "Log.enable" => client.domains.log = true,
            "Page.enable" => client.domains.page = true,
            "DOM.enable" | "CSS.enable" | "Overlay.enable" => client.domains.dom = true,
            "Network.enable" => client.domains.network = true,
            "WebMCP.enable" => client.domains.webmcp = true,
            _ => {},
        }
    }

    fn replay(&mut self, client: u64) -> Vec<Value> {
        let Some(state) = self.clients.get(&client).map(|client| ClientView {
            target: client.target.clone(),
            domains: client.domains,
        }) else {
            return Vec::new();
        };
        let Some(target) = self.targets.get(&state.target) else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        if state.domains.runtime {
            if let Some(context) = target.context_id {
                messages.push(execution_context(context, &target.origin, &target.url));
            }
        }
        if state.domains.log || state.domains.runtime {
            messages.extend(target.console.iter().cloned());
        }
        if state.domains.page {
            messages.push(event(
                "Page.frameNavigated",
                json!({ "frame": self.frame_body(target) }),
            ));
        }
        if state.domains.webmcp {
            messages.extend(tool_events(target, &[]));
        }
        messages
    }

    fn script(&mut self, message: ScriptToDevtoolsControlMsg) -> Vec<(u64, Value)> {
        match message {
            ScriptToDevtoolsControlMsg::NewGlobal(
                (browsing_context, pipeline, _, webview),
                sender,
                info,
            ) => self.install_global(browsing_context, pipeline, webview, sender, info),
            ScriptToDevtoolsControlMsg::Navigate(
                browsing_context,
                NavigationState::Stop(pipeline, info),
            ) => self.navigate_stop(browsing_context, pipeline, info),
            ScriptToDevtoolsControlMsg::Navigate(_, NavigationState::Start(_)) => Vec::new(),
            ScriptToDevtoolsControlMsg::ConsoleAPI(pipeline, message, _) => {
                self.console_message(pipeline, &message)
            },
            ScriptToDevtoolsControlMsg::ReportPageError(pipeline, error) => {
                self.page_error(pipeline, &error)
            },
            ScriptToDevtoolsControlMsg::ClearConsole(pipeline, _) => self.clear_console(pipeline),
            ScriptToDevtoolsControlMsg::WebMcpTools(pipeline, tools) => {
                self.replace_tools(pipeline, tools)
            },
            ScriptToDevtoolsControlMsg::TitleChanged(pipeline, title) => {
                if let Some(key) = self.pipelines.get(&pipeline).cloned() {
                    if let Some(target) = self.targets.get_mut(&key) {
                        target.title = title;
                    }
                }
                Vec::new()
            },
            _ => Vec::new(),
        }
    }

    fn install_global(
        &mut self,
        browsing_context: servo_base::id::BrowsingContextId,
        pipeline: PipelineId,
        webview: WebViewId,
        sender: GenericSender<devtools_traits::DevtoolScriptControlMsg>,
        info: DevtoolsPageInfo,
    ) -> Vec<(u64, Value)> {
        if !info.is_top_level_global {
            // A child frame shares the webview id. Adopting it would replace
            // the page the inspector is attached to.
            return Vec::new();
        }
        let key = target_key(webview);
        let origin = origin_of(&info.url);
        let fresh = !self.targets.contains_key(&key);
        let target = self.targets.entry(key.clone()).or_insert_with(|| Target {
            webview,
            browsing_context: browsing_context.to_string(),
            pipeline: None,
            context_id: None,
            sender: None,
            title: String::new(),
            url: String::new(),
            origin: String::new(),
            top_level: info.is_top_level_global,
            console: VecDeque::new(),
            nodes: HashMap::new(),
            node_ids: HashMap::new(),
            next_node: 1,
            requests: HashMap::new(),
            request_order: VecDeque::new(),
            tools: Vec::new(),
        });
        target.sender = Some(sender);
        target.browsing_context = browsing_context.to_string();
        if info.is_top_level_global {
            target.top_level = true;
            target.origin = origin;
        }
        let _ = fresh;
        self.note_pipeline(&key, pipeline, info)
    }

    fn navigate_stop(
        &mut self,
        browsing_context: servo_base::id::BrowsingContextId,
        pipeline: PipelineId,
        info: DevtoolsPageInfo,
    ) -> Vec<(u64, Value)> {
        let Some(key) = self
            .targets
            .iter()
            .find(|(_, target)| target.browsing_context == browsing_context.to_string())
            .map(|(key, _)| key.clone())
        else {
            return Vec::new();
        };
        self.note_pipeline(&key, pipeline, info)
    }

    fn note_pipeline(
        &mut self,
        key: &str,
        pipeline: PipelineId,
        info: DevtoolsPageInfo,
    ) -> Vec<(u64, Value)> {
        if !info.is_top_level_global {
            return Vec::new();
        }
        let Some(target) = self.targets.get_mut(key) else {
            return Vec::new();
        };
        target.title = info.title.clone();
        target.url = info.url.to_string();
        if info.is_top_level_global {
            target.origin = origin_of(&info.url);
        }
        if target.pipeline == Some(pipeline) {
            return Vec::new();
        }
        if let Some(previous) = target.pipeline {
            self.pipelines.remove(&previous);
        }
        let previous_context = target.context_id;
        target.pipeline = Some(pipeline);
        target.nodes.clear();
        target.node_ids.clear();
        target.tools.clear();
        target.requests.clear();
        target.request_order.clear();
        let context = self.next_context;
        self.next_context += 1;
        target.context_id = Some(context);
        self.pipelines.insert(pipeline, key.to_owned());
        let origin = target.origin.clone();
        let url = target.url.clone();
        let frame_body = json!({
            "id": target.browsing_context.clone(),
            "loaderId": target.pipeline.map(|pipeline| pipeline.to_string()).unwrap_or_default(),
            "url": target.url.clone(),
            "securityOrigin": target.origin.clone(),
            "mimeType": "text/html",
        });
        let frame = event("Page.frameNavigated", json!({ "frame": frame_body }));
        let loaded = event("Page.loadEventFired", json!({ "timestamp": 0 }));
        let mut messages = Vec::new();
        if let Some(previous_context) = previous_context {
            messages.push(event(
                "Runtime.executionContextDestroyed",
                json!({ "executionContextId": previous_context }),
            ));
        }
        messages.push(execution_context(context, &origin, &url));
        messages.push(frame);
        messages.push(loaded);
        self.fanout(key, messages, |domains, message| {
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "Runtime.executionContextCreated" | "Runtime.executionContextDestroyed" => {
                    domains.runtime
                },
                "Page.frameNavigated" | "Page.loadEventFired" => domains.page,
                _ => false,
            }
        })
    }

    fn console_message(
        &mut self,
        pipeline: PipelineId,
        message: &ConsoleMessage,
    ) -> Vec<(u64, Value)> {
        let Some(key) = self.pipelines.get(&pipeline).cloned() else {
            return Vec::new();
        };
        let text = console_text(&message.arguments);
        let level = cdp_level(&message.fields.level);
        let runtime = event(
            "Runtime.consoleAPICalled",
            json!({
                "type": level,
                "args": message.arguments.iter().map(remote_object).collect::<Vec<_>>(),
                "executionContextId": self.targets.get(&key).and_then(|target| target.context_id).unwrap_or(0),
                "timestamp": message.fields.time_stamp,
                "stackTrace": stack(message),
            }),
        );
        let log = event(
            "Log.entryAdded",
            json!({
                "entry": {
                    "source": "javascript",
                    "level": log_level(&message.fields.level),
                    "text": text,
                    "timestamp": message.fields.time_stamp,
                    "url": message.fields.filename,
                    "lineNumber": message.fields.line_number,
                }
            }),
        );
        self.buffer(&key, log.clone());
        self.fanout(&key, vec![runtime, log], |domains, message| {
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            (method == "Runtime.consoleAPICalled" && domains.runtime)
                || (method == "Log.entryAdded" && domains.log)
        })
    }

    fn page_error(&mut self, pipeline: PipelineId, error: &PageError) -> Vec<(u64, Value)> {
        let Some(key) = self.pipelines.get(&pipeline).cloned() else {
            return Vec::new();
        };
        let entry = event(
            "Log.entryAdded",
            json!({
                "entry": {
                    "source": "javascript",
                    "level": "error",
                    "text": error.error_message,
                    "timestamp": error.time_stamp,
                    "url": error.source_name,
                    "lineNumber": error.line_number,
                }
            }),
        );
        let runtime = event(
            "Runtime.exceptionThrown",
            json!({
                "timestamp": error.time_stamp,
                "exceptionDetails": {
                    "text": error.error_message,
                    "lineNumber": error.line_number,
                    "columnNumber": error.column_number,
                    "exception": { "type": "object", "subtype": "error", "description": error.error_message },
                }
            }),
        );
        self.buffer(&key, entry.clone());
        self.fanout(&key, vec![runtime, entry], |domains, message| {
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            (method == "Runtime.exceptionThrown" && domains.runtime)
                || (method == "Log.entryAdded" && domains.log)
        })
    }

    fn clear_console(&mut self, pipeline: PipelineId) -> Vec<(u64, Value)> {
        let Some(key) = self.pipelines.get(&pipeline).cloned() else {
            return Vec::new();
        };
        if let Some(target) = self.targets.get_mut(&key) {
            target.console.clear();
        }
        self.fanout(
            &key,
            vec![event("Log.entryAdded", json!({ "entry": { "source": "javascript", "level": "info", "text": "console.clear", "timestamp": 0 } }))],
            |domains, _| domains.log,
        )
    }

    fn replace_tools(
        &mut self,
        pipeline: PipelineId,
        tools: Vec<WebMcpToolRecord>,
    ) -> Vec<(u64, Value)> {
        let Some(key) = self.pipelines.get(&pipeline).cloned() else {
            return Vec::new();
        };
        let Some(target) = self.targets.get_mut(&key) else {
            return Vec::new();
        };
        let frame = pipeline.to_string();
        let previous: Vec<(String, String)> = target
            .tools
            .iter()
            .filter(|tool| tool.origin == target.origin && tool.frame_id == frame)
            .map(|tool| (tool.name.clone(), tool.frame_id.clone()))
            .collect();
        target.tools.retain(|tool| tool.frame_id != frame);
        target.tools.extend(tools);
        let events = tool_events(target, &previous);
        self.fanout(&key, events, |domains, _| domains.webmcp)
    }

    fn network(&mut self, id: String, event: NetworkEvent) -> Vec<(u64, Value)> {
        let browsing = match &event {
            NetworkEvent::HttpRequest(request) => request.browsing_context_id.to_string(),
            NetworkEvent::HttpRequestUpdate(request) => request.browsing_context_id.to_string(),
            NetworkEvent::HttpResponse(response) => response.browsing_context_id.to_string(),
            NetworkEvent::SecurityInfo(update) => update.browsing_context_id.to_string(),
            NetworkEvent::Failed {
                browsing_context_id,
                ..
            } => browsing_context_id.to_string(),
        };
        let Some(key) = self
            .targets
            .iter()
            .find(|(_, target)| target.browsing_context == browsing)
            .map(|(key, _)| key.clone())
        else {
            return Vec::new();
        };
        if !self.network_open(&key) {
            return Vec::new();
        }
        let messages = match event {
            NetworkEvent::HttpRequest(request) | NetworkEvent::HttpRequestUpdate(request) => {
                let target = self.targets.get_mut(&key).unwrap();
                if !target.requests.contains_key(&id) {
                    target.request_order.push_back(id.clone());
                }
                let entry = target.requests.entry(id.clone()).or_insert(NetRequest {
                    events_sent: false,
                    response_sent: false,
                    finished: false,
                    body: None,
                });
                if entry.events_sent {
                    return Vec::new();
                }
                entry.events_sent = true;
                let url = request.url.to_string();
                vec![event_value(
                    "Network.requestWillBeSent",
                    json!({
                        "requestId": id,
                        "loaderId": id,
                        "frameId": request.browsing_context_id.to_string(),
                        "documentURL": url,
                        "timestamp": timestamp_secs(request.started_date_time),
                        "wallTime": timestamp_secs(request.started_date_time),
                        "initiator": { "type": "other" },
                        "type": resource_type(request.destination, request.is_xhr),
                        "request": {
                            "url": url,
                            "method": request.method.to_string(),
                            "headers": headers_json(&request.headers),
                        },
                    }),
                )]
            },
            NetworkEvent::HttpResponse(response) => {
                let target = self.targets.get_mut(&key).unwrap();
                if !target.requests.contains_key(&id) {
                    target.request_order.push_back(id.clone());
                }
                let entry = target.requests.entry(id.clone()).or_insert(NetRequest {
                    events_sent: true,
                    response_sent: false,
                    finished: false,
                    body: None,
                });
                if let Some(body) = response.body {
                    if body.len() <= NETWORK_BODY_CAP {
                        entry.body = Some(body.to_vec());
                    }
                }
                let mut messages = Vec::new();
                if !entry.response_sent {
                    entry.response_sent = true;
                    let mime = response
                        .headers
                        .as_ref()
                        .and_then(|headers| headers.get(http::header::CONTENT_TYPE))
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    messages.push(event_value(
                        "Network.responseReceived",
                        json!({
                            "requestId": id,
                            "loaderId": id,
                            "frameId": response.browsing_context_id.to_string(),
                            "timestamp": 0,
                            "type": "Other",
                            "response": {
                                "url": "",
                                "status": response.status.code().as_u16(),
                                "statusText": String::from_utf8_lossy(response.status.message()).to_string(),
                                "headers": response.headers.as_ref().map(headers_json).unwrap_or(json!({})),
                                "mimeType": mime,
                                "encodedDataLength": entry.body.as_ref().map(|body| body.len()).unwrap_or(0),
                            },
                        }),
                    ));
                }
                if entry.body.is_some() && !entry.finished {
                    entry.finished = true;
                    messages.push(event_value(
                        "Network.loadingFinished",
                        json!({ "requestId": id, "timestamp": 0, "encodedDataLength": entry.body.as_ref().map(|body| body.len()).unwrap_or(0) }),
                    ));
                }
                messages
            },
            NetworkEvent::Failed { error, .. } => {
                vec![event_value(
                    "Network.loadingFailed",
                    json!({ "requestId": id, "timestamp": 0, "type": "other", "errorText": error, "canceled": false }),
                )]
            },
            NetworkEvent::SecurityInfo(_) => Vec::new(),
        };
        if let Some(target) = self.targets.get_mut(&key) {
            evict_requests(target);
        }
        self.fanout(&key, messages, |domains, _| domains.network)
    }

    fn network_open(&self, key: &str) -> bool {
        self.clients
            .values()
            .any(|client| client.target == key && client.domains.network)
    }

    fn evaluate(&mut self, id: Option<i64>, target_key: &str, params: &Value) -> CommandOutcome {
        let Some(id) = id else {
            return CommandOutcome::Messages(Vec::new());
        };
        if let Some(requested) = params.get("contextId").and_then(Value::as_i64) {
            let current = self
                .targets
                .get(target_key)
                .and_then(|target| target.context_id);
            if current != Some(requested) {
                return CommandOutcome::Messages(error(
                    Some(id),
                    -32000,
                    "execution context destroyed",
                ));
            }
        }
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        self.pending(Some(id), target_key, |pipeline| ScriptRequest::Eval {
            pipeline,
            expression,
        })
    }

    fn invoke_tool(&mut self, id: Option<i64>, target_key: &str, params: &Value) -> CommandOutcome {
        let Some(id) = id else {
            return CommandOutcome::Messages(Vec::new());
        };
        let name = params
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if let Some(frame) = params.get("frameId").and_then(Value::as_str) {
            let top = self
                .targets
                .get(target_key)
                .and_then(|target| target.pipeline)
                .map(|pipeline| pipeline.to_string());
            if top.as_deref() != Some(frame) {
                return CommandOutcome::Messages(error(
                    Some(id),
                    -32000,
                    "only the top-level document is inspected",
                ));
            }
        }
        let arguments = params
            .get("input")
            .map(|value| value.to_string())
            .unwrap_or_else(|| "{}".to_owned());
        let invocation = format!("invoke-{id}");
        // The command result arrives with the script reply.
        self.pending(Some(id), target_key, |pipeline| ScriptRequest::InvokeTool {
            pipeline,
            invocation,
            name,
            arguments,
        })
    }

    fn pending(
        &mut self,
        id: Option<i64>,
        target_key: &str,
        build: impl FnOnce(PipelineId) -> ScriptRequest,
    ) -> CommandOutcome {
        let Some(id) = id else {
            return CommandOutcome::Messages(Vec::new());
        };
        let Some(target) = self.targets.get(target_key) else {
            return CommandOutcome::Messages(error(Some(id), -32000, "no target"));
        };
        let (Some(pipeline), Some(sender)) = (target.pipeline, target.sender.clone()) else {
            return CommandOutcome::Messages(error(Some(id), -32000, "no document"));
        };
        CommandOutcome::Pending {
            id,
            sender,
            request: build(pipeline),
        }
    }

    fn response_body(&self, target_key: &str, request_id: &str) -> Value {
        let body = self
            .targets
            .get(target_key)
            .and_then(|target| target.requests.get(request_id))
            .and_then(|request| request.body.clone());
        match body {
            Some(body) => match String::from_utf8(body.clone()) {
                Ok(text) => json!({ "body": text, "base64Encoded": false }),
                Err(_) => json!({
                    "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, body),
                    "base64Encoded": true
                }),
            },
            None => json!({ "body": "", "base64Encoded": false }),
        }
    }

    fn frame(&self, target_key: &str) -> Value {
        json!({ "frame": self.targets.get(target_key).map(|target| self.frame_body(target)).unwrap_or(json!({})) })
    }

    fn frame_body(&self, target: &Target) -> Value {
        json!({
            "id": target.browsing_context,
            "loaderId": target.pipeline.map(|pipeline| pipeline.to_string()).unwrap_or_default(),
            "url": target.url,
            "securityOrigin": target.origin,
            "mimeType": "text/html",
        })
    }

    fn adopt_node(&mut self, target_key: &str, node: &NodeInfo, document: bool) -> Value {
        let Some(target) = self.targets.get_mut(target_key) else {
            return Value::Null;
        };
        let node_id = if let Some(id) = target.node_ids.get(&node.unique_id) {
            *id
        } else {
            let id = target.next_node;
            target.next_node += 1;
            target.node_ids.insert(node.unique_id.clone(), id);
            target.nodes.insert(id, node.unique_id.clone());
            id
        };
        let mut attributes = Vec::new();
        for attribute in &node.attrs {
            attributes.push(Value::String(attribute.name.clone()));
            attributes.push(Value::String(attribute.value.clone()));
        }
        json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": node.node_type,
            "nodeName": node.node_name,
            "localName": node.node_name.to_ascii_lowercase(),
            "nodeValue": node.node_value,
            "childNodeCount": node.num_children,
            "attributes": attributes,
            "documentURL": if document { target.url.clone() } else { String::new() },
            "baseURL": node.base_uri,
        })
    }

    fn buffer(&mut self, key: &str, message: Value) {
        let Some(target) = self.targets.get_mut(key) else {
            return;
        };
        target.console.push_back(message);
        while target.console.len() > CONSOLE_BUFFER {
            target.console.pop_front();
        }
    }

    fn fanout(
        &self,
        key: &str,
        messages: Vec<Value>,
        allow: impl Fn(&Domains, &Value) -> bool,
    ) -> Vec<(u64, Value)> {
        let mut out = Vec::new();
        for (id, client) in &self.clients {
            if client.target != key {
                continue;
            }
            for message in &messages {
                if allow(&client.domains, message) {
                    out.push((*id, message.clone()));
                }
            }
        }
        out
    }
}

fn self_node(session: &Session, target: &str, node: i64) -> String {
    session
        .targets
        .get(target)
        .and_then(|target| target.nodes.get(&node))
        .cloned()
        .unwrap_or_default()
}

fn tool_events(target: &Target, previous: &[(String, String)]) -> Vec<Value> {
    let visible: Vec<&WebMcpToolRecord> = target
        .tools
        .iter()
        .filter(|tool| tool.origin == target.origin || target.origin.is_empty())
        .collect();
    let names: Vec<&str> = visible.iter().map(|tool| tool.name.as_str()).collect();
    let mut messages = Vec::new();
    let removed: Vec<&(String, String)> = previous
        .iter()
        .filter(|(name, _)| !names.contains(&name.as_str()))
        .collect();
    if !removed.is_empty() {
        let tools: Vec<Value> = removed
            .iter()
            .map(|(name, frame)| json!({ "name": name, "frameId": frame }))
            .collect();
        messages.push(event("WebMCP.toolsRemoved", json!({ "tools": tools })));
    }
    let added: Vec<Value> = visible
        .iter()
        .filter(|tool| !previous.iter().any(|(name, _)| name == &tool.name))
        .map(|tool| {
            let schema = serde_json::from_str::<Value>(&tool.input_schema).unwrap_or(Value::Null);
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": schema,
                "frameId": tool.frame_id,
            })
        })
        .collect();
    if !added.is_empty() {
        messages.push(event("WebMCP.toolsAdded", json!({ "tools": added })));
    }
    messages
}

pub(crate) enum ScriptReply {
    Eval(EvaluateJSReply),
    Root(Option<NodeInfo>),
    Children(Option<Vec<NodeInfo>>),
    ChildrenOf {
        parent: i64,
        nodes: Option<Vec<NodeInfo>>,
    },
    Computed(Option<Vec<NodeStyle>>),
    Tool(String, devtools_traits::WebMcpInvokeResult),
    Empty,
    Missing,
}

fn method_enables(method: &str) -> bool {
    matches!(
        method,
        "Runtime.enable"
            | "Log.enable"
            | "Page.enable"
            | "DOM.enable"
            | "CSS.enable"
            | "Overlay.enable"
            | "Network.enable"
            | "WebMCP.enable"
    )
}

fn target_key(webview: WebViewId) -> String {
    webview.to_string()
}

fn origin_of(url: &ServoUrl) -> String {
    url.origin().ascii_serialization().into_owned()
}

fn execution_context(id: i64, origin: &str, url: &str) -> Value {
    event(
        "Runtime.executionContextCreated",
        json!({
            "context": {
                "id": id,
                "origin": origin,
                "name": url,
                "uniqueId": id.to_string(),
                "auxData": { "isDefault": true, "type": "default" },
            }
        }),
    )
}

fn event(method: &str, params: Value) -> Value {
    event_value(method, params)
}

fn event_value(method: &str, params: Value) -> Value {
    json!({ "method": method, "params": params })
}

fn success(id: Option<i64>, result: Value) -> Vec<Value> {
    match id {
        Some(id) => vec![json!({ "id": id, "result": result })],
        None => Vec::new(),
    }
}

fn error(id: Option<i64>, code: i64, message: &str) -> Vec<Value> {
    match id {
        Some(id) => vec![json!({ "id": id, "error": { "code": code, "message": message } })],
        None => Vec::new(),
    }
}

fn remote_object(value: &DebuggerValue) -> Value {
    match value {
        DebuggerValue::VoidValue => json!({ "type": "undefined" }),
        DebuggerValue::NullValue(_) => {
            json!({ "type": "object", "subtype": "null", "value": null })
        },
        DebuggerValue::BooleanValue(value) => json!({ "type": "boolean", "value": value }),
        DebuggerValue::NumberValue(value) => json!({ "type": "number", "value": value }),
        DebuggerValue::StringValue(value) => json!({ "type": "string", "value": value }),
        DebuggerValue::ObjectValue { class, preview, .. } => json!({
            "type": "object",
            "className": class,
            "description": class,
            "preview": preview.as_ref().map(|preview| json!({ "type": "object", "description": preview.kind })),
        }),
    }
}

fn eval_result(reply: &EvaluateJSReply) -> Value {
    if reply.has_exception {
        json!({
            "result": { "type": "object", "subtype": "error", "description": reply.exception_message.clone().unwrap_or_default() },
            "exceptionDetails": {
                "text": reply.exception_message.clone().unwrap_or_else(|| "exception".to_owned()),
                "exception": remote_object(&reply.value),
            }
        })
    } else {
        json!({ "result": remote_object(&reply.value) })
    }
}

fn console_text(arguments: &[DebuggerValue]) -> String {
    arguments
        .iter()
        .map(|argument| match argument {
            DebuggerValue::StringValue(value) => value.clone(),
            DebuggerValue::NumberValue(value) => value.to_string(),
            DebuggerValue::BooleanValue(value) => value.to_string(),
            DebuggerValue::NullValue(_) => "null".to_owned(),
            DebuggerValue::VoidValue => "undefined".to_owned(),
            DebuggerValue::ObjectValue { class, .. } => class.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn cdp_level(level: &ConsoleLogLevel) -> &'static str {
    match level {
        ConsoleLogLevel::Debug => "debug",
        ConsoleLogLevel::Info => "info",
        ConsoleLogLevel::Warn => "warning",
        ConsoleLogLevel::Error => "error",
        ConsoleLogLevel::Trace => "trace",
        ConsoleLogLevel::Dir => "dir",
        ConsoleLogLevel::Log => "log",
    }
}

fn log_level(level: &ConsoleLogLevel) -> &'static str {
    match level {
        ConsoleLogLevel::Warn => "warning",
        ConsoleLogLevel::Error => "error",
        ConsoleLogLevel::Debug => "verbose",
        ConsoleLogLevel::Info
        | ConsoleLogLevel::Log
        | ConsoleLogLevel::Trace
        | ConsoleLogLevel::Dir => "info",
    }
}

fn stack(message: &ConsoleMessage) -> Value {
    let frames = message.stacktrace.as_ref().map(|frames| {
        frames
            .iter()
            .map(|frame| {
                json!({
                    "functionName": frame.function_name,
                    "url": frame.filename,
                    "lineNumber": frame.line_number,
                    "columnNumber": frame.column_number,
                })
            })
            .collect::<Vec<_>>()
    });
    json!({ "callFrames": frames.unwrap_or_default() })
}

fn headers_json(headers: &HeaderMap) -> Value {
    let mut object = serde_json::Map::new();
    for (name, value) in headers {
        object.insert(
            name.to_string(),
            Value::String(value.to_str().unwrap_or("").to_owned()),
        );
    }
    Value::Object(object)
}

fn resource_type(destination: Destination, is_xhr: bool) -> &'static str {
    if is_xhr {
        return "XHR";
    }
    match destination {
        Destination::Document => "Document",
        Destination::Script | Destination::Worker | Destination::SharedWorker => "Script",
        Destination::Style => "Stylesheet",
        Destination::Image => "Image",
        Destination::Font => "Font",
        _ => "Other",
    }
}

fn evict_requests(target: &mut Target) {
    let mut bytes: usize = target
        .requests
        .values()
        .map(|request| request.body.as_ref().map(Vec::len).unwrap_or(0))
        .sum();
    while target.requests.len() > MAX_REQUESTS || bytes > MAX_BODY_BYTES {
        let Some(id) = target.request_order.pop_front() else {
            break;
        };
        if let Some(old) = target.requests.remove(&id) {
            bytes = bytes.saturating_sub(old.body.as_ref().map(Vec::len).unwrap_or(0));
        }
    }
}

fn timestamp_secs(time: std::time::SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

struct ClientView {
    target: String,
    domains: Domains,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use devtools_traits::{
        ConsoleMessageFields, DebuggerValue, HttpRequest as DevtoolsHttpRequest, WebMcpToolRecord,
        get_time_stamp,
    };
    use http::{HeaderMap, Method};
    use net_traits::request::Destination;
    use servo_base::generic_channel;
    use servo_base::id::{
        BrowsingContextId, PipelineId, TEST_BROWSING_CONTEXT_ID, TEST_PIPELINE_ID, TEST_WEBVIEW_ID,
    };
    use servo_url::ServoUrl;

    use super::*;

    fn page(url: &str) -> DevtoolsPageInfo {
        DevtoolsPageInfo {
            title: "Example".into(),
            url: ServoUrl::parse(url).unwrap(),
            is_top_level_global: true,
            is_service_worker: false,
        }
    }

    fn global(
        pipeline: PipelineId,
        sender: GenericSender<devtools_traits::DevtoolScriptControlMsg>,
    ) -> DevtoolsControlMsg {
        DevtoolsControlMsg::FromScript(ScriptToDevtoolsControlMsg::NewGlobal(
            (TEST_BROWSING_CONTEXT_ID, pipeline, None, TEST_WEBVIEW_ID),
            sender,
            page("https://example.com/"),
        ))
    }

    fn other_pipeline() -> PipelineId {
        PipelineId {
            namespace_id: TEST_PIPELINE_ID.namespace_id,
            index: servo_base::id::Index::new(9).unwrap(),
        }
    }

    #[test]
    fn console_survives_navigation_and_rejects_unknown_methods() {
        let (sender, _receiver) = generic_channel::channel().unwrap();
        let mut session = Session::new();
        let client = session.attach(TEST_WEBVIEW_ID.to_string());
        assert!(
            session
                .control(global(TEST_PIPELINE_ID, sender.clone()))
                .is_empty()
        );
        let enabled = session.command(client, &json!({ "id": 1, "method": "Runtime.enable" }));
        let CommandOutcome::Messages(messages) = enabled else {
            panic!("enable is immediate");
        };
        assert_eq!(messages[0]["id"], 1);
        assert_eq!(messages[1]["method"], "Runtime.executionContextCreated");
        let first_context = messages[1]["params"]["context"]["id"].as_i64().unwrap();

        let unknown = session.command(
            client,
            &json!({ "id": 2, "method": "HeapProfiler.takeHeapSnapshot" }),
        );
        let CommandOutcome::Messages(messages) = unknown else {
            panic!("unknown method");
        };
        assert_eq!(messages[0]["error"]["code"], -32601);

        let logged = ConsoleMessage {
            fields: ConsoleMessageFields {
                level: ConsoleLogLevel::Log,
                filename: "app.js".into(),
                line_number: 4,
                column_number: 1,
                time_stamp: get_time_stamp(),
            },
            arguments: vec![DebuggerValue::StringValue("hello".into())],
            stacktrace: None,
        };
        let events = session.control(DevtoolsControlMsg::FromScript(
            ScriptToDevtoolsControlMsg::ConsoleAPI(TEST_PIPELINE_ID, logged, None),
        ));
        assert!(
            events
                .iter()
                .any(|(_, message)| message["method"] == "Runtime.consoleAPICalled")
        );
        assert_eq!(events[0].1["params"]["args"][0]["value"], "hello");

        let events = session.control(global(other_pipeline(), sender));
        assert!(events.iter().any(|(_, message)| {
            message["method"] == "Runtime.executionContextDestroyed"
                && message["params"]["executionContextId"] == first_context
        }));
        assert!(
            events
                .iter()
                .any(|(_, message)| message["method"] == "Runtime.executionContextCreated")
        );
        let stale = session.control(DevtoolsControlMsg::FromScript(
            ScriptToDevtoolsControlMsg::ConsoleAPI(
                TEST_PIPELINE_ID,
                ConsoleMessage {
                    fields: ConsoleMessageFields {
                        level: ConsoleLogLevel::Error,
                        filename: String::new(),
                        line_number: 0,
                        column_number: 0,
                        time_stamp: 0,
                    },
                    arguments: vec![DebuggerValue::StringValue("stale".into())],
                    stacktrace: None,
                },
                None,
            ),
        ));
        assert!(stale.is_empty());
    }

    #[test]
    fn network_failure_and_same_origin_tools() {
        let (sender, _receiver) = generic_channel::channel().unwrap();
        let mut session = Session::new();
        let client = session.attach(TEST_WEBVIEW_ID.to_string());
        session.control(global(TEST_PIPELINE_ID, sender));
        session.command(client, &json!({ "id": 1, "method": "Network.enable" }));
        session.command(client, &json!({ "id": 2, "method": "WebMCP.enable" }));
        let request = DevtoolsHttpRequest {
            url: ServoUrl::parse("https://example.com/app.js").unwrap(),
            method: Method::GET,
            headers: HeaderMap::new(),
            body: None,
            pipeline_id: TEST_PIPELINE_ID,
            started_date_time: SystemTime::now(),
            time_stamp: 0,
            connect_time: Duration::from_millis(1),
            send_time: Duration::from_millis(1),
            destination: Destination::Script,
            is_xhr: false,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
        };
        let events = session.control(DevtoolsControlMsg::FromChrome(
            ChromeToDevtoolsControlMsg::NetworkEvent(
                "req-1".into(),
                NetworkEvent::HttpRequest(request),
            ),
        ));
        assert_eq!(events[0].1["method"], "Network.requestWillBeSent");
        assert_eq!(events[0].1["params"]["type"], "Script");
        let events = session.control(DevtoolsControlMsg::FromChrome(
            ChromeToDevtoolsControlMsg::NetworkEvent(
                "req-1".into(),
                NetworkEvent::Failed {
                    pipeline_id: TEST_PIPELINE_ID,
                    browsing_context_id: TEST_BROWSING_CONTEXT_ID,
                    error: "connection reset".into(),
                },
            ),
        ));
        assert_eq!(events[0].1["method"], "Network.loadingFailed");

        let frame = TEST_PIPELINE_ID.to_string();
        let events = session.control(DevtoolsControlMsg::FromScript(
            ScriptToDevtoolsControlMsg::WebMcpTools(
                TEST_PIPELINE_ID,
                vec![
                    WebMcpToolRecord {
                        name: "search_flights".into(),
                        description: "Search".into(),
                        input_schema: "{\"type\":\"object\"}".into(),
                        origin: "https://example.com".into(),
                        frame_id: frame.clone(),
                    },
                    WebMcpToolRecord {
                        name: "foreign".into(),
                        description: "Hidden".into(),
                        input_schema: "{}".into(),
                        origin: "https://other.example".into(),
                        frame_id: frame,
                    },
                ],
            ),
        ));
        let added = events
            .iter()
            .find(|(_, message)| message["method"] == "WebMCP.toolsAdded")
            .unwrap();
        assert_eq!(added.1["params"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(added.1["params"]["tools"][0]["name"], "search_flights");
        assert_eq!(
            added.1["params"]["tools"][0]["inputSchema"]["type"],
            "object"
        );
        assert_eq!(
            added.1["params"]["tools"][0]["frameId"],
            TEST_PIPELINE_ID.to_string()
        );
    }

    #[test]
    fn an_iframe_does_not_replace_the_top_level_document() {
        let (sender, _receiver) = generic_channel::channel().unwrap();
        let mut session = Session::new();
        session.set_listen(9333, "secret".into());
        let client = session.attach(TEST_WEBVIEW_ID.to_string());
        session.control(global(TEST_PIPELINE_ID, sender.clone()));
        session.command(client, &json!({ "id": 1, "method": "Runtime.enable" }));
        let mut frame = page("https://example.com/embed");
        frame.title = "Embed".into();
        frame.is_top_level_global = false;
        let events = session.control(DevtoolsControlMsg::FromScript(
            ScriptToDevtoolsControlMsg::NewGlobal(
                (
                    TEST_BROWSING_CONTEXT_ID,
                    other_pipeline(),
                    None,
                    TEST_WEBVIEW_ID,
                ),
                sender,
                frame,
            ),
        ));
        assert!(events.is_empty());
        let listed = session.targets();
        assert_eq!(listed[0]["url"], "https://example.com/");
        assert_eq!(listed[0]["title"], "Example");
        assert!(
            listed[0]["webSocketDebuggerUrl"]
                .as_str()
                .unwrap()
                .contains("ws://127.0.0.1:9333/devtools/page/")
        );
        assert!(
            listed[0]["webSocketDebuggerUrl"]
                .as_str()
                .unwrap()
                .contains("token=secret")
        );
        let stale = session.command(
            client,
            &json!({
                "id": 4,
                "method": "Runtime.evaluate",
                "params": { "expression": "document.title", "contextId": 999 }
            }),
        );
        let CommandOutcome::Messages(messages) = stale else {
            panic!("stale context is rejected immediately");
        };
        assert_eq!(messages[0]["error"]["code"], -32000);
    }

    #[test]
    fn network_is_not_retained_until_a_client_is_listening() {
        let (sender, _receiver) = generic_channel::channel().unwrap();
        let mut session = Session::new();
        let client = session.attach(TEST_WEBVIEW_ID.to_string());
        session.control(global(TEST_PIPELINE_ID, sender));
        let request = DevtoolsHttpRequest {
            url: ServoUrl::parse("https://example.com/a").unwrap(),
            method: Method::GET,
            headers: HeaderMap::new(),
            body: None,
            pipeline_id: TEST_PIPELINE_ID,
            started_date_time: SystemTime::now(),
            time_stamp: 0,
            connect_time: Duration::from_millis(1),
            send_time: Duration::from_millis(1),
            destination: Destination::Document,
            is_xhr: false,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
        };
        let ignored = session.control(DevtoolsControlMsg::FromChrome(
            ChromeToDevtoolsControlMsg::NetworkEvent(
                "early".into(),
                NetworkEvent::HttpRequest(request),
            ),
        ));
        assert!(ignored.is_empty());
        session.detach(client);
    }

    #[test]
    fn buffered_console_is_replayed_on_enable() {
        let (sender, _receiver) = generic_channel::channel().unwrap();
        let mut session = Session::new();
        let client = session.attach(TEST_WEBVIEW_ID.to_string());
        session.control(global(TEST_PIPELINE_ID, sender));
        session.control(DevtoolsControlMsg::FromScript(
            ScriptToDevtoolsControlMsg::ConsoleAPI(
                TEST_PIPELINE_ID,
                ConsoleMessage {
                    fields: ConsoleMessageFields {
                        level: ConsoleLogLevel::Warn,
                        filename: String::new(),
                        line_number: 0,
                        column_number: 0,
                        time_stamp: 1,
                    },
                    arguments: vec![DebuggerValue::StringValue("early".into())],
                    stacktrace: None,
                },
                None,
            ),
        ));
        let CommandOutcome::Messages(messages) =
            session.command(client, &json!({ "id": 1, "method": "Log.enable" }))
        else {
            panic!("enable");
        };
        assert!(
            messages
                .iter()
                .any(|message| message["method"] == "Log.entryAdded"
                    && message["params"]["entry"]["text"] == "early")
        );
    }

    fn _pipeline_is_copy(pipeline: PipelineId, browsing: BrowsingContextId) {
        let _ = (pipeline, browsing);
    }
}
