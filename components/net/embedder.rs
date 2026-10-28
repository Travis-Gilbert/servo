/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::PathBuf;

use cookie::Cookie;
use embedder_traits::{
    AuthenticationResponse, EmbedderControlId, FilePickerRequest, WebResourceRequest,
    WebResourceResponseMsg,
};
use net_traits::CookieOperationId;
use net_traits::request::RequestId;
use servo_base::id::WebViewId;
use servo_url::ServoUrl;
use tokio::sync::mpsc::UnboundedSender as TokioSender;
use tokio::sync::oneshot::Sender as TokioOneshotSender;

/// Messages sent from the network threads to the embedder.
pub enum NetToEmbedderMsg {
    /// Open file dialog to select files. Set boolean flag to true allows to select multiple files.
    SelectFiles(
        EmbedderControlId,
        FilePickerRequest,
        TokioOneshotSender<Option<Vec<PathBuf>>>,
    ),
    WebResourceRequested(
        Option<WebViewId>,
        WebResourceRequest,
        TokioSender<WebResourceResponseMsg>,
    ),
    /// A fetch for this webview has started. Sent before the fetch runs, and
    /// followed by exactly one [`RequestFinished`] with the same id on this
    /// channel. The embedder drops either message when that webview is gone.
    RequestStarted(WebViewId, RequestId, WebResourceRequest),
    /// The fetch reported by [`RequestStarted`] has ended: its response body
    /// completed, it failed, or it was cancelled.
    RequestFinished(WebViewId, RequestId),
    /// Request authentication for a load or navigation from the embedder.
    RequestAuthentication(
        WebViewId,
        ServoUrl,
        bool, /* for proxy */
        TokioOneshotSender<Option<AuthenticationResponse>>,
    ),
    /// Response to an asynchronous request from SiteDataManager with cookies result.
    EmbedderCookieOperationResponseWithCookies(CookieOperationId, Vec<Cookie<'static>>),
    /// Response to an asynchronous request from SiteDataManager.
    EmbedderCookieOperationResponse(CookieOperationId),
}
