/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{
    InsecureRequestsPolicy, Origin, Referrer, RequestBuilder, RequestClient,
};
use serde_json::json;
use servo_url::ServoUrl;

const TEST_URL: &str = "https://app.invalid/";

fn request_client(is_window: bool, is_nested_browsing_context: bool) -> RequestClient {
    RequestClient {
        preloaded_resources: Default::default(),
        policy_container: Default::default(),
        origin: Origin::Origin(ServoUrl::parse(TEST_URL).unwrap().origin()),
        is_nested_browsing_context,
        insecure_requests_policy: InsecureRequestsPolicy::DoNotUpgrade,
        has_trustworthy_ancestor_origin: true,
        is_window,
    }
}

#[test]
fn missing_request_client_kind_deserializes_as_non_window() {
    for nested in [false, true] {
        let expected = request_client(true, nested);
        let mut encoded = serde_json::to_value(&expected).unwrap();
        assert_eq!(
            encoded.as_object_mut().unwrap().remove("is_window"),
            Some(json!(true))
        );

        let decoded: RequestClient = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.is_window);
        assert_eq!(decoded.is_nested_browsing_context, nested);
        assert_eq!(decoded.origin, expected.origin);
        assert!(decoded.has_trustworthy_ancestor_origin);
    }
}

#[test]
fn request_client_kind_roundtrip_is_independent_of_nesting() {
    for is_window in [false, true] {
        for nested in [false, true] {
            let expected = request_client(is_window, nested);
            let encoded = serde_json::to_value(&expected).unwrap();
            assert_eq!(encoded["is_window"], json!(is_window));

            let decoded: RequestClient = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.is_window, is_window);
            assert_eq!(decoded.is_nested_browsing_context, nested);
            assert_eq!(decoded.origin, expected.origin);
        }
    }
}

#[test]
fn non_boolean_request_client_kind_is_rejected() {
    for invalid_kind in [
        json!(null),
        json!(0),
        json!(1),
        json!("false"),
        json!("true"),
        json!([]),
        json!({}),
    ] {
        let mut encoded = serde_json::to_value(request_client(true, false)).unwrap();
        encoded["is_window"] = invalid_kind.clone();
        assert!(
            serde_json::from_value::<RequestClient>(encoded).is_err(),
            "accepted non-boolean caller kind: {invalid_kind}"
        );
    }
}

#[test]
fn request_builder_preserves_request_client_kind() {
    for is_window in [false, true] {
        for nested in [false, true] {
            let expected = request_client(is_window, nested);
            let request = RequestBuilder::new(
                None,
                UrlWithBlobClaim::new(ServoUrl::parse(TEST_URL).unwrap(), None),
                Referrer::NoReferrer,
            )
            .client(expected.clone())
            .build();

            let client = request.clone().client.unwrap();
            assert_eq!(client.is_window, is_window);
            assert_eq!(client.is_nested_browsing_context, nested);
            assert_eq!(client.origin, expected.origin);
        }
    }
}
