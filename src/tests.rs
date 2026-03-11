#[cfg(test)]
mod integration {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use tower::ServiceExt;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::{
        config::Config,
        proxy::{build_router, AppState},
    };

    fn make_state(mock_url: String, keys: Vec<&str>) -> AppState {
        let config = Config {
            keys: keys.into_iter().map(String::from).collect(),
            port: 0,
            host: "127.0.0.1".into(),
            cooldown_seconds: 60,
            request_timeout_ms: 5000,
            cache_ttl_seconds: 300,
            cache_max_entries: 1000,
            cache_max_body_bytes: 32 * 1024,
            log_level: "error".into(),
            log_format: "text".into(),
            ipinfo_base_url: mock_url,
        };
        AppState::new(config)
    }

    fn empty_req(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proxy_get_ip_returns_upstream_body() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/8.8.8.8"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ip":"8.8.8.8","country":"US"}"#)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&mock)
            .await;

        let router = build_router(make_state(mock.uri(), vec!["test-key"]));
        let resp = router.oneshot(empty_req("/8.8.8.8")).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["country"], "US");
    }

    #[tokio::test]
    async fn cache_hit_on_second_request() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1.1.1.1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ip":"1.1.1.1","country":"AU"}"#)
                    .insert_header("content-type", "application/json"),
            )
            // Exactly one upstream call — second request must be served from cache
            .expect(1)
            .mount(&mock)
            .await;

        let state = make_state(mock.uri(), vec!["key"]);
        let router = build_router(state);

        let r1 = router.clone().oneshot(empty_req("/1.1.1.1")).await.unwrap();
        let r2 = router.oneshot(empty_req("/1.1.1.1")).await.unwrap();

        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(r2.status(), StatusCode::OK);
        mock.verify().await;
    }

    #[tokio::test]
    async fn no_cache_header_bypasses_cache() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.2.2.2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ip":"2.2.2.2"}"#)
                    .insert_header("content-type", "application/json"),
            )
            // Both requests must reach upstream
            .expect(2)
            .mount(&mock)
            .await;

        let state = make_state(mock.uri(), vec!["key"]);
        let router = build_router(state);

        for _ in 0..2 {
            let req = axum::http::Request::builder()
                .uri("/2.2.2.2")
                .header("cache-control", "no-cache")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        mock.verify().await;
    }

    #[tokio::test]
    async fn returns_503_when_all_keys_exhausted() {
        let state = make_state("http://127.0.0.1:1".into(), vec!["only-key"]);
        state.rotator.mark_cooldown("only-key");

        let router = build_router(state);
        let resp = router.oneshot(empty_req("/8.8.8.8")).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().contains_key("retry-after"));
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let mock = MockServer::start().await;
        let router = build_router(make_state(mock.uri(), vec!["key"]));
        let resp = router.oneshot(empty_req("/health")).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn delete_cache_returns_204() {
        let mock = MockServer::start().await;
        let router = build_router(make_state(mock.uri(), vec!["key"]));
        let req = axum::http::Request::builder()
            .method("DELETE")
            .uri("/cache")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn upstream_429_marks_key_cooldown() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3.3.3.3"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&mock)
            .await;

        let state = make_state(mock.uri(), vec!["rate-limited-key"]);
        let router = build_router(state.clone());

        let _ = router.oneshot(empty_req("/3.3.3.3")).await.unwrap();

        let stats = state.rotator.stats();
        assert_eq!(stats.cooling, 1);
        assert_eq!(stats.active, 0);
    }

    #[tokio::test]
    async fn content_type_forwarded_from_upstream() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/4.4.4.4/country"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("US")
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&mock)
            .await;

        let router = build_router(make_state(mock.uri(), vec!["key"]));
        let resp = router.oneshot(empty_req("/4.4.4.4/country")).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/plain"));
    }
}
