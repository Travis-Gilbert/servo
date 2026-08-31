/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use bytes::Bytes;
use content_security_policy::Destination;
use embedder_traits::{
    GenericEmbedderProxy, WebResourceRequest, WebResourceResponse, WebResourceResponseMsg,
};
use headers::{ContentLength, HeaderMapExt};
use http::header::TRANSFER_ENCODING;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::Response;
use hyper::ext::ReasonPhrase;
use net_traits::NetworkError;
use net_traits::request::Request;
use servo_config::pref;

use crate::connector::BoxedBody;
use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::FetchContext;

#[derive(Clone)]
pub struct RequestInterceptor {
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
}

impl RequestInterceptor {
    pub fn new(embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>) -> RequestInterceptor {
        RequestInterceptor { embedder_proxy }
    }

    /// Supply an HTTP transport response without bypassing Fetch policy checks.
    /// An unclaimed request continues to the network; an invalid claimed response fails closed.
    pub async fn intercept_request(
        &self,
        request: &Request,
        context: &FetchContext,
    ) -> Result<Option<Response<BoxedBody>>, NetworkError> {
        if context.cancellation_listener.cancelled() {
            return Err(NetworkError::LoadCancelled);
        }

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let is_for_main_frame = matches!(request.destination, Destination::Document);
        let web_resource_request = WebResourceRequest {
            has_body: request.body.is_some(),
            method: request.method.clone(),
            url: request.current_url().into_url(),
            headers: request.headers.clone(),
            destination: request.destination,
            referrer_url: request.referrer.to_url().map(|url| url.as_url().clone()),
            is_for_main_frame,
            is_redirect: request.redirect_count > 0,
        };

        self.embedder_proxy
            .send(NetToEmbedderMsg::WebResourceRequested(
                request.target_webview_id,
                web_resource_request,
                sender,
            ));

        let mut response: Option<WebResourceResponse> = None;
        let mut expected_length = None;
        let mut has_body = true;
        let mut accumulated_body = Vec::new();
        let response_limit = pref!(network_interception_max_response_bytes);
        loop {
            let message = tokio::select! {
                biased;
                _ = context.cancellation_listener.wait_until_cancelled() => {
                    return Err(NetworkError::LoadCancelled);
                },
                message = receiver.recv() => message,
            };
            let Some(message) = message else {
                // A disconnected embedder that never claimed this request is equivalent to
                // DoNotIntercept. Once claimed, missing completion must not fetch from a socket.
                return if response.is_none() {
                    Ok(None)
                } else {
                    Err(invalid_response(
                        "response channel closed before completion",
                    ))
                };
            };
            match message {
                WebResourceResponseMsg::Start(webresource_response) => {
                    if response.is_some() {
                        return Err(invalid_response("duplicate response headers"));
                    }
                    // WebResourceRequest does not expose upload bytes. Do not pretend a claimed
                    // request with a body was delivered; custom protocol handlers remain body-aware.
                    if request.body.is_some() {
                        return Err(invalid_response(
                            "request body is not available to the interceptor",
                        ));
                    }
                    let mut response_url = webresource_response.url.clone();
                    let mut request_url = request.current_url().into_url();
                    response_url.set_fragment(None);
                    request_url.set_fragment(None);
                    if response_url != request_url {
                        return Err(invalid_response(
                            "response URL does not match the current request",
                        ));
                    }
                    if webresource_response.status_code.is_informational() {
                        return Err(invalid_response("interim HTTP responses are not supported"));
                    }
                    let length = webresource_response
                        .headers
                        .typed_try_get::<ContentLength>()
                        .map_err(|_| invalid_response("invalid or conflicting Content-Length"))?
                        .map(|length| length.0);
                    if length.is_some()
                        && webresource_response.headers.contains_key(TRANSFER_ENCODING)
                    {
                        return Err(invalid_response(
                            "Content-Length conflicts with Transfer-Encoding",
                        ));
                    }
                    has_body = request.method != Method::HEAD
                        && !matches!(
                            webresource_response.status_code,
                            StatusCode::NO_CONTENT
                                | StatusCode::RESET_CONTENT
                                | StatusCode::NOT_MODIFIED
                        );
                    if !has_body
                        && request.method != Method::HEAD
                        && webresource_response.status_code != StatusCode::NOT_MODIFIED
                        && length.is_some_and(|length| length != 0)
                    {
                        return Err(invalid_response(
                            "bodyless response declares a nonempty body",
                        ));
                    }
                    if has_body && length.is_some_and(|length| length > response_limit) {
                        return Err(invalid_response(
                            "response exceeds the configured byte limit",
                        ));
                    }
                    expected_length = has_body.then_some(length).flatten();
                    response = Some(webresource_response);
                },
                WebResourceResponseMsg::SendBodyData(data) => {
                    if response.is_none() {
                        return Err(invalid_response(
                            "body data arrived before response headers",
                        ));
                    }
                    if !has_body && !data.is_empty() {
                        return Err(invalid_response("body data is forbidden for this response"));
                    }
                    if accumulated_body
                        .len()
                        .checked_add(data.len())
                        .is_none_or(|length| length as u64 > response_limit)
                    {
                        return Err(invalid_response(
                            "response exceeds the configured byte limit",
                        ));
                    }
                    accumulated_body
                        .try_reserve(data.len())
                        .map_err(|_| invalid_response("could not allocate response body"))?;
                    accumulated_body.extend_from_slice(&data);
                },
                WebResourceResponseMsg::FinishLoad => {
                    let response = response.ok_or_else(|| {
                        invalid_response("completion arrived before response headers")
                    })?;
                    if expected_length.is_some_and(|length| length != accumulated_body.len() as u64)
                    {
                        return Err(invalid_response(
                            "response body does not match Content-Length",
                        ));
                    }
                    let reason = ReasonPhrase::try_from(response.status_message)
                        .map_err(|_| invalid_response("invalid HTTP status message"))?;
                    let body = Full::new(Bytes::from(accumulated_body))
                        .map_err(|never| match never {})
                        .boxed();
                    let mut transport_response = Response::new(body);
                    *transport_response.status_mut() = response.status_code;
                    *transport_response.headers_mut() = response.headers;
                    transport_response.extensions_mut().insert(reason);
                    return Ok(Some(transport_response));
                },
                WebResourceResponseMsg::CancelLoad => {
                    return Err(NetworkError::LoadCancelled);
                },
                WebResourceResponseMsg::DoNotIntercept => {
                    return if response.is_none() {
                        Ok(None)
                    } else {
                        Err(invalid_response(
                            "claimed response cannot fall back to the network",
                        ))
                    };
                },
            }
        }
    }
}

fn invalid_response(message: &str) -> NetworkError {
    NetworkError::ResourceLoadError(format!("Invalid intercepted response: {message}"))
}
