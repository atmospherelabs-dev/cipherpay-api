use super::*;
use crate::repair_tests::{invoice, merchant, pool};
use zcash_address::unified::{Encoding, Fvk, Ufvk};

fn asset() -> Asset {
    Asset {
        asset_id: "nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near".into(),
        symbol: "USDC".into(),
        blockchain: "base".into(),
        decimals: 6,
        contract_address: Some("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".into()),
    }
}

async fn fixture() -> (SqlitePool, crate::invoices::Invoice) {
    let pool = pool().await;
    merchant(&pool, "m").await;
    invoice(&pool, "i", "m").await;
    let sk = orchard::keys::SpendingKey::from_bytes([7; 32]).unwrap();
    let key = Ufvk::try_from_items(vec![Fvk::Orchard(
        orchard::keys::FullViewingKey::from(&sk).to_bytes(),
    )])
    .unwrap()
    .encode(&zcash_protocol::consensus::NetworkType::Main);
    let address = crate::addresses::derive_invoice_address(&key, 1).unwrap();
    sqlx::query("UPDATE invoices SET payment_address=?,orchard_receiver_hex=?,price_zatoshis=10000000 WHERE id='i'")
        .bind(address.ua_string).bind(address.orchard_receiver_hex).execute(&pool).await.unwrap();
    let i = crate::invoices::get_invoice(&pool, "i")
        .await
        .unwrap()
        .unwrap();
    (pool, i)
}

fn input() -> QuoteInput {
    QuoteInput {
        asset_id: asset().asset_id,
        refund_address: "0x1234567890123456789012345678901234567890".into(),
    }
}

#[tokio::test]
async fn quote_is_exact_and_server_bound() {
    let (_, i) = fixture().await;
    let body = quote_body(&i, &input(), &asset(), true).unwrap();
    assert_eq!(body["amount"], "10000000");
    assert_eq!(body["recipient"], i.payment_address);
    assert_eq!(body["swapType"], "EXACT_OUTPUT");
    assert_eq!(body["destinationAsset"], ZEC);
    assert_eq!(body["confidentiality"], "basic");
    assert!(body.get("appFees").is_none());
    assert!(serde_json::from_value::<QuoteInput>(
        json!({"asset_id":"x","refund_address":"x","amount":"1"})
    )
    .is_err());
    for status in ["draft", "confirmed", "expired", "underpaid"] {
        let mut invalid = i.clone();
        invalid.status = status.into();
        assert!(quote_body(&invalid, &input(), &asset(), true).is_err());
    }
    let mut legacy = i.clone();
    legacy.orchard_receiver_hex = None;
    assert!(quote_body(&legacy, &input(), &asset(), true).is_err());
}

#[tokio::test]
async fn rejects_underpayment_and_substituted_recipient() {
    let (_, i) = fixture().await;
    let body = quote_body(&i, &input(), &asset(), true).unwrap();
    let mut response = json!({"quoteRequest":body,"quote":{"amountIn":"12345678","amountOut":"10000000","minAmountOut":"10000000"}});
    assert!(checked_quote(&response, &body, &asset(), true).is_ok());
    response["quote"]["minAmountOut"] = json!("9950000");
    assert!(checked_quote(&response, &body, &asset(), true).is_err());
    response["quote"]["minAmountOut"] = json!("10000000");
    response["quoteRequest"]["recipient"] = json!("attacker");
    assert!(checked_quote(&response, &body, &asset(), true).is_err());
}

#[tokio::test]
async fn confidential_quotes_reject_public_or_unconfirmed_routing() {
    let (_, i) = fixture().await;
    for dry in [true, false] {
        let body = quote_body(&i, &input(), &asset(), dry).unwrap();
        let response = json!({"quoteRequest":body,"quote":{
            "amountIn":"12345678","amountOut":"10000000","minAmountOut":"10000000",
            "depositAddress":input().refund_address,"deadline":body["deadline"]
        }});
        let quote = checked_quote(&response, &body, &asset(), dry).unwrap();
        assert_eq!(quote["confidentiality"], "basic");
        for mode in [json!("public"), json!("advanced"), Value::Null] {
            let mut changed = response.clone();
            changed["quoteRequest"]["confidentiality"] = mode;
            assert!(checked_quote(&changed, &body, &asset(), dry).is_err());
        }
        let mut missing = response;
        missing["quoteRequest"].as_object_mut().unwrap().remove("confidentiality");
        assert!(checked_quote(&missing, &body, &asset(), dry).is_err());
        missing.as_object_mut().unwrap().remove("quoteRequest");
        assert!(checked_quote(&missing, &body, &asset(), dry).is_err());
    }
}

#[actix_web::test]
async fn confidential_gateway_never_retries_with_public_routing() {
    use actix_web::{App, HttpServer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = web::Data::new(AtomicUsize::new(0));
    let cc = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = HttpServer::new(move || App::new().app_data(cc.clone()).route("/quote", web::post().to(
        |body: web::Json<Value>, calls: web::Data<AtomicUsize>| async move {
            assert_eq!(body["confidentiality"], "basic");
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            if attempt == 2 { return HttpResponse::ServiceUnavailable().finish(); }
            let mut echo = body.into_inner();
            if attempt == 0 { echo["confidentiality"] = json!("public"); }
            else if attempt == 1 { echo.as_object_mut().unwrap().remove("confidentiality"); }
            HttpResponse::Ok().json(signatures::signed(json!({"quoteRequest":echo,"quote":{
                "amountIn":"12345678","amountOut":"10000000","minAmountOut":"10000000"
            }})))
        }
    ))).listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let gateway = Gateway {
        enabled: true, fee_recipient: None, key: String::new(), base,
        client: reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(), tokens: Mutex::new(None),
        eligibility: Mutex::new(HashMap::new()), eligibility_slots: Semaphore::new(4),
    };
    let (_, invoice) = fixture().await;
    let body = quote_body(&invoice, &input(), &asset(), true).unwrap();
    for expected_calls in 1..=3 {
        assert!(gateway.quote(&body).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    assert!(gateway.quote(&body).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    handle.stop(true).await;
}

#[tokio::test]
async fn reservation_is_durable_unique_and_requires_full_payment() {
    let (p, i) = fixture().await;
    let deadline = (Utc::now() + Duration::minutes(30)).to_rfc3339();
    let (a, b) = tokio::join!(
        reserve(&p, "i", &deadline, None),
        reserve(&p, "i", &deadline, None)
    );
    assert_eq!(a.unwrap() as u8 + b.unwrap() as u8, 1);
    let strict = crate::invoices::get_invoice(&p, "i")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(strict.minimum_payment_zatoshis(), 10000000);
    assert_eq!(i.minimum_payment_zatoshis(), 9950000);
    let pending = crate::invoices::get_pending_invoices(&p).await.unwrap();
    assert!(pending[0].exact_payment);
    assert_eq!(
        read_attempt(&p, "i", "").await.unwrap().unwrap()["state"],
        "CREATING"
    );
    assert!(!reserve(&p, "i", &deadline, None).await.unwrap());
    sqlx::query("DELETE FROM invoices WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    assert!(read_attempt(&p, "i", "").await.unwrap().is_none());
}

#[tokio::test]
async fn encrypted_attempt_survives_reload_without_changing_invoice_status() {
    let (p, _) = fixture().await;
    let key = "11".repeat(32);
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::minutes(30)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    let payload = crypto_payload(&key);
    sqlx::query("UPDATE crosschain_attempts SET state='SUCCESS',payload=? WHERE invoice_id='i'")
        .bind(&payload)
        .execute(&p)
        .await
        .unwrap();
    assert!(!payload.contains("refundAddress"));
    let restored = read_attempt(&p, "i", &key).await.unwrap().unwrap();
    assert_eq!(restored["quote"]["refundAddress"], "private");
    assert_eq!(
        crate::invoices::get_invoice(&p, "i")
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    assert!(read_attempt(&p, "i", &"22".repeat(32)).await.is_err());
}

fn crypto_payload(key: &str) -> String {
    crate::crypto::encrypt(&json!({"refundAddress":"private"}).to_string(), key).unwrap()
}

#[test]
fn asset_and_refund_networks_are_restricted() {
    assert!(curated(&asset().asset_id));
    assert!(!curated("nep141:fake-usdc.near"));
    assert!(!curated("1cs_v1:sol:wrapped-zec"));
    assert!(valid_refund("base", &input().refund_address));
    assert!(!valid_refund("tron", &input().refund_address));
    assert!(!valid_refund(
        "eth",
        "0x0000000000000000000000000000000000000000"
    ));
    assert!(!valid_refund("btc", "tb1qtestnet"));
    assert!(positive_units(&json!("1.5")).is_err());
    assert!(positive_units(&json!("-1")).is_err());
}

#[tokio::test]
#[ignore = "prints a public test-fixture address for dry quote probes; never fund"]
async fn dry_probe_address() {
    let (_, i) = fixture().await;
    println!("DRY_PROBE_ADDRESS={}", i.payment_address);
}

#[actix_web::test]
async fn wallet_lock_requires_ownership_and_survives_reload() {
    use actix_web::{test, App};
    let (p, _) = fixture().await;
    let mut config = crate::config::Config::from_env().unwrap();
    config.encryption_key = "11".repeat(32);
    let deadline = (Utc::now() + Duration::minutes(30)).to_rfc3339();
    reserve(&p, "i", &deadline, None).await.unwrap();
    let payload = crate::crypto::encrypt(
        &json!({"deadline":deadline}).to_string(),
        &config.encryption_key,
    )
    .unwrap();
    sqlx::query("UPDATE crosschain_attempts SET state='PENDING_DEPOSIT',payload=?,last_success=strftime('%s','now') WHERE invoice_id='i'").bind(payload).execute(&p).await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/{id}", web::post().to(wallet_send)),
    )
    .await;
    let access = security::token(&p, "i").await.unwrap();
    assert_eq!(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/i")
                .set_json(json!({"action":"start"}))
                .to_request()
        )
        .await
        .status(),
        403
    );
    let req = || {
        test::TestRequest::post()
            .uri("/i")
            .insert_header(("X-Checkout-Token", access.clone()))
    };
    let first: Value =
        test::call_and_read_body_json(&app, req().set_json(json!({"action":"start"})).to_request())
            .await;
    let token = first["token"].as_str().unwrap();
    assert_eq!(
        test::call_service(&app, req().set_json(json!({"action":"start"})).to_request())
            .await
            .status(),
        409
    );
    assert_eq!(
        read_attempt(&p, "i", &config.encryption_key)
            .await
            .unwrap()
            .unwrap()["sendStarted"],
        true
    );
    assert_eq!(
        test::call_service(
            &app,
            req()
                .set_json(json!({"action":"rejected","token":uuid::Uuid::new_v4().to_string()}))
                .to_request()
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        test::call_service(
            &app,
            req()
                .set_json(json!({"action":"rejected","token":token}))
                .to_request()
        )
        .await
        .status(),
        200
    );
    let second: Value =
        test::call_and_read_body_json(&app, req().set_json(json!({"action":"start"})).to_request())
            .await;
    let token = second["token"].as_str().unwrap();
    assert_eq!(
        test::call_service(
            &app,
            req()
                .set_json(json!({"action":"submitted","token":token,"txid":"a".repeat(64)}))
                .to_request()
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        test::call_service(
            &app,
            req()
                .set_json(json!({"action":"rejected","token":token}))
                .to_request()
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        crate::invoices::get_invoice(&p, "i")
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
}

#[tokio::test]
async fn strict_settlement_cannot_be_bypassed_by_a_stale_scanner_snapshot() {
    let (p, _) = fixture().await;
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::minutes(30)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    assert!(!crate::invoices::mark_detected(&p, "i", "tx", 9999999)
        .await
        .unwrap());
    assert!(crate::invoices::mark_detected(&p, "i", "tx", 10000000)
        .await
        .unwrap());
    sqlx::query("UPDATE invoices SET received_zatoshis=9999999 WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    assert!(!crate::invoices::mark_confirmed(&p, "i", None, None)
        .await
        .unwrap());
    sqlx::query("UPDATE invoices SET received_zatoshis=10000000 WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    assert!(crate::invoices::mark_confirmed(&p, "i", None, None)
        .await
        .unwrap());
}

#[actix_web::test]
async fn http_flow_creates_once_and_provider_success_does_not_settle() {
    use actix_web::{test, App, HttpServer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let quote_calls = web::Data::new(AtomicUsize::new(0));
    let calls = quote_calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = HttpServer::new(move || App::new().app_data(calls.clone())
        .route("/quote", web::post().to(|body: web::Json<Value>, calls: web::Data<AtomicUsize>| async move {
            if body["dry"] == false { calls.fetch_add(1,Ordering::SeqCst); }
            assert_eq!(body["appFees"],json!([{"recipient":"cipherscan.near","fee":125}]));
            assert_eq!(body["confidentiality"], "basic");
            let mut echo=body.0.clone();
            echo["appFees"]=json!([{"recipient":"cipherscan.near","fee":125},{"recipient":"provider.near","fee":20}]);
            HttpResponse::Ok().json(signatures::signed(json!({"correlationId":"test-quote","quoteRequest":echo,"quote":{
                "amountIn":"130000000","amountOut":"10000000","minAmountOut":"10000000",
                "depositAddress":"0x2345678901234567890123456789012345678901","deadline":(chrono::DateTime::parse_from_rfc3339(body["deadline"].as_str().unwrap()).unwrap()+Duration::days(3)).to_rfc3339(),"timeEstimate":150
            }})))
        }))
        .route("/status", web::get().to(|| async { HttpResponse::Ok().json(json!({"status":"SUCCESS"})) })))
        .listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let (p, _) = fixture().await;
    let mut config = crate::config::Config::from_env().unwrap();
    config.encryption_key = "11".repeat(32);
    let gateway = Gateway {
        enabled: true,
        fee_recipient: Some("cipherscan.near".into()),
        key: String::new(),
        base,
        client: reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),
        tokens: Mutex::new(Some((Instant::now(), vec![asset()]))),
        eligibility: Mutex::new(HashMap::new()),
        eligibility_slots: Semaphore::new(4),
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(gateway))
            .route("/{id}", web::post().to(create))
            .route("/{id}", web::get().to(status))
            .route("/{id}/preview", web::post().to(preview)),
    )
    .await;
    let input = json!({"asset_id":asset().asset_id,"refund_address":input().refund_address});
    let preview_response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/i/preview")
            .set_json(&input)
            .to_request(),
    )
    .await;
    assert_eq!(preview_response.status(), 200);
    assert_eq!(quote_calls.load(Ordering::SeqCst), 0);
    assert!(read_attempt(&p, "i", "").await.unwrap().is_none());
    let first: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::post()
            .uri("/i")
            .insert_header(("X-Checkout-Token", security::token(&p, "i").await.unwrap()))
            .set_json(&input)
            .to_request(),
    )
    .await;
    assert_eq!(first["state"], "PENDING_DEPOSIT");
    assert_eq!(first["quote"]["confidentiality"], "basic");
    let fee_terms: (i64, i64) = sqlx::query_as("SELECT total_bps,cipherpay_bps FROM crosschain_fees WHERE invoice_id='i'")
        .fetch_one(&p).await.unwrap();
    assert_eq!(fee_terms, (145, 125));
    let funding_deadline =
        chrono::DateTime::parse_from_rfc3339(first["quote"]["deadline"].as_str().unwrap()).unwrap();
    let provider_deadline =
        chrono::DateTime::parse_from_rfc3339(first["quote"]["providerDeadline"].as_str().unwrap())
            .unwrap();
    assert!(funding_deadline < Utc::now() + Duration::minutes(31));
    assert!(provider_deadline > Utc::now() + Duration::days(2));
    let invoice = crate::invoices::get_invoice(&p, "i")
        .await
        .unwrap()
        .unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&invoice.expires_at).unwrap()
            == chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z").unwrap()
    );

    let second: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::post()
            .uri("/i")
            .insert_header(("X-Checkout-Token", security::token(&p, "i").await.unwrap()))
            .set_json(&input)
            .to_request(),
    )
    .await;
    assert_eq!(first["quote"], second["quote"]);
    assert_eq!(quote_calls.load(Ordering::SeqCst), 1);
    let status: Value =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/i").to_request()).await;
    assert_eq!(status["state"], "PENDING_DEPOSIT");
    assert!(status.get("quote").is_none());
    assert!(status.get("sentTxid").is_none());
    assert_eq!(status["asset"]["symbol"],"USDC");
    assert_eq!(
        crate::invoices::get_invoice(&p, "i")
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    let fee: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state,correlation_id,received_units FROM crosschain_fees WHERE invoice_id='i'",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(fee, ("pending".into(), Some("test-quote".into()), None));
    crate::billing::create_fee_entry(&p, "i", "m", 0.001, 0.01)
        .await
        .unwrap();
    let billed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fee_ledger WHERE invoice_id='i'")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(billed, 0);
    handle.stop(true).await;
}

#[actix_web::test]
async fn minimum_quote_error_is_actionable_without_exposing_provider_payload_or_creating_attempt() {
    use actix_web::{test, App, HttpServer};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = HttpServer::new(|| {
        App::new().route(
            "/quote",
            web::post().to(|| async {
                HttpResponse::BadRequest().json(json!({
                    "message":"Amount is too low for bridge, try at least 100000",
                    "private_debug":"recipient-and-refund-data-must-not-leak"
                }))
            }),
        )
    })
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let (p, _) = fixture().await;
    sqlx::query("UPDATE invoices SET price_zatoshis=75326 WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    let gateway = Gateway {
        enabled: true,
        fee_recipient: None,
        key: String::new(),
        base,
        client: reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),
        tokens: Mutex::new(Some((Instant::now(), vec![asset()]))),
        eligibility: Mutex::new(HashMap::new()),
        eligibility_slots: Semaphore::new(4),
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .app_data(web::Data::new(gateway))
            .route("/{id}/preview", web::post().to(preview)),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/i/preview")
            .set_json(json!({"asset_id":asset().asset_id,"refund_address":input().refund_address}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 400);
    let body: Value = test::read_body_json(response).await;
    assert_eq!(body["code"], "BELOW_MINIMUM_OUTPUT");
    assert_eq!(body["minimumOutputZatoshis"], "100000");
    assert!(!body.to_string().contains("recipient-and-refund"));
    assert!(read_attempt(&p, "i", "").await.unwrap().is_none());
    for message in [
        "Amount is too low for bridge, try at least 100000 recipient=secret",
        "Amount is too low for bridge, try at least 0",
        "Other failure",
    ] {
        assert!(minimum_output_error(
            reqwest::StatusCode::BAD_REQUEST,
            &json!({"message":message})
        )
        .is_none());
    }
    handle.stop(true).await;
}

#[actix_web::test]
async fn eligibility_checks_routes_without_wallet_or_attempt_and_distinguishes_outages() {
    use actix_web::{test, App, HttpServer};
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    let calls = web::Data::new(AtomicUsize::new(0));
    let state = web::Data::new(AtomicU8::new(0));
    let cc = calls.clone();
    let ss = state.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = HttpServer::new(move || App::new().app_data(cc.clone()).app_data(ss.clone()).route("/quote",web::post().to(|body:web::Json<Value>, calls:web::Data<AtomicUsize>, state:web::Data<AtomicU8>| async move {
        calls.fetch_add(1,Ordering::SeqCst);
        assert_eq!(body["dry"],true);
        assert_eq!(body["swapType"],"EXACT_OUTPUT");
        assert_eq!(body["confidentiality"], "basic");
        assert!(body["refundTo"].as_str().is_some());
        let mode=state.load(Ordering::SeqCst);
        if mode==3 { return HttpResponse::BadRequest().json(json!({"message":"Temporary swap limits: minimum swap amount is $100"})); }
        if mode==2 { return HttpResponse::ServiceUnavailable().json(json!({"message":"upstream unavailable"})); }
        if mode==0 || body["originAsset"].as_str().unwrap().contains("base-") {
            return HttpResponse::BadRequest().json(json!({"message":"Amount is too low for bridge, try at least 100000"}));
        }
        HttpResponse::Ok().json(signatures::signed(json!({"quoteRequest":body.0,"quote":{"amountIn":"5000000","amountOut":body["amount"],"minAmountOut":body["amount"]}})))
    }))).listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let (p, _) = fixture().await;
    sqlx::query("UPDATE invoices SET price_zatoshis=75326 WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    let mut sol = asset();
    sol.asset_id = "nep141:sol.omft.near".into();
    sol.blockchain = "sol".into();
    sol.symbol = "SOL".into();
    let gateway = web::Data::new(Gateway {
        enabled: true,
        fee_recipient: None,
        key: String::new(),
        base,
        client: reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),
        tokens: Mutex::new(Some((Instant::now(), vec![asset(), sol]))),
        eligibility: Mutex::new(HashMap::new()),
        eligibility_slots: Semaphore::new(4),
    });
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .app_data(gateway.clone())
            .route("/{id}/eligibility", web::get().to(eligibility)),
    )
    .await;
    for mode in [0, 1, 2, 3] {
        state.store(mode as u8, Ordering::SeqCst);
        gateway.eligibility.lock().await.clear();
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/i/eligibility").to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(
            body["state"],
            ["UNAVAILABLE", "AVAILABLE", "UNKNOWN", "UNAVAILABLE"][mode]
        );
        assert_eq!(
            body["routes"][0]["state"],
            if mode == 2 {
                "UNKNOWN"
            } else {
                "BELOW_MINIMUM"
            }
        );
        if mode == 1 {
            assert_eq!(body["routes"][1]["state"], "AVAILABLE");
        }
        if mode == 3 {
            assert_eq!(body["routes"][0]["minimumSwapUsd"], "100");
        }
        for invalid in ["0", "100 secret", "-100", "100.000", "1.2.3"] {
            assert!(minimum_swap_usd_error(reqwest::StatusCode::BAD_REQUEST, &json!({"message":format!("Temporary swap limits: minimum swap amount is ${invalid}")})).is_none());
        }
        assert!(!body.to_string().contains("refundTo"));
        assert!(!body.to_string().contains("depositAddress"));
        let before = calls.load(Ordering::SeqCst);
        let _: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/i/eligibility").to_request(),
        )
        .await;
        assert_eq!(
            before,
            calls.load(Ordering::SeqCst),
            "cached check should not repeat provider calls"
        );
        assert!(read_attempt(&p, "i", "").await.unwrap().is_none());
    }
    // Invoice amount changes invalidate cached eligibility.
    state.store(1, Ordering::SeqCst);
    sqlx::query("UPDATE invoices SET price_zatoshis=200000 WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    let before = calls.load(Ordering::SeqCst);
    let _: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get().uri("/i/eligibility").to_request(),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), before + 2);
    // Existing attempts win even over a cached available result.
    sqlx::query("INSERT INTO crosschain_attempts(invoice_id,state) VALUES ('i','CREATING')")
        .execute(&p)
        .await
        .unwrap();
    let before = calls.load(Ordering::SeqCst);
    let response = test::call_service(
        &app,
        test::TestRequest::get().uri("/i/eligibility").to_request(),
    )
    .await;
    assert_eq!(response.status(), 409);
    assert_eq!(before, calls.load(Ordering::SeqCst));
    handle.stop(true).await;
}

#[tokio::test]
async fn committed_quote_extension_keeps_funding_window_bounded() {
    let (_, i) = fixture().await;
    let body = quote_body(&i, &input(), &asset(), false).unwrap();
    let requested =
        chrono::DateTime::parse_from_rfc3339(body["deadline"].as_str().unwrap()).unwrap();
    let provider = (requested + Duration::days(3)).to_rfc3339();
    let mut response = json!({"quoteRequest":body,"quote":{"amountIn":"5490787","amountOut":"10000000","minAmountOut":"10000000","depositAddress":input().refund_address,"deadline":provider,"timeWhenInactive":provider}});
    let checked = checked_quote(&response, &body, &asset(), false).unwrap();
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(checked["deadline"].as_str().unwrap()).unwrap(),
        requested
    );
    assert_eq!(checked["providerDeadline"], provider);
    let earlier =
        (Utc::now() + Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    response["quote"]["timeWhenInactive"] = json!(earlier);
    assert_eq!(
        checked_quote(&response, &body, &asset(), false).unwrap()["deadline"],
        earlier
    );
    response["quote"]["timeWhenInactive"] = json!((Utc::now() - Duration::seconds(1)).to_rfc3339());
    assert!(checked_quote(&response, &body, &asset(), false).is_err());
    response["quote"]["timeWhenInactive"] = Value::Null;
    for deadline in [
        (Utc::now() - Duration::seconds(1)).to_rfc3339(),
        (requested + Duration::days(8)).to_rfc3339(),
        "invalid".into(),
    ] {
        response["quote"]["deadline"] = json!(deadline);
        assert!(checked_quote(&response, &body, &asset(), false).is_err());
    }
}

#[actix_web::test]
async fn public_invoice_marks_crosschain_without_exposing_swap_details() {
    use actix_web::{test, App};
    let (p, _) = fixture().await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .route("/{id}", web::get().to(crate::api::invoices::get)),
    )
    .await;
    let before: Value =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/i").to_request()).await;
    assert_eq!(before["is_crosschain"], false);
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::minutes(30)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    let after: Value =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/i").to_request()).await;
    assert_eq!(after["is_crosschain"], true);
    assert!(after.get("payload").is_none());
    assert!(after.get("depositAddress").is_none());
    assert!(after.get("refundAddress").is_none());
}

#[actix_web::test]
async fn merchant_invoice_tag_is_scoped_and_preserves_received_amount() {
    use actix_web::{cookie::Cookie, test, App};
    let (p, _) = fixture().await;
    merchant(&p, "other").await;
    invoice(&p, "other-invoice", "other").await;
    invoice(&p, "direct", "m").await;
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::minutes(30)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE invoices SET received_zatoshis=10007000,status='confirmed' WHERE id='i'")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions(id,merchant_id,expires_at) VALUES ('test-session','m','2099-01-01T00:00:00Z')").execute(&p).await.unwrap();
    let config = crate::config::Config::from_env().unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p))
            .app_data(web::Data::new(config))
            .route("/", web::get().to(crate::api::auth::my_invoices)),
    )
    .await;
    let rows: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/")
            .cookie(Cookie::new("cpay_session", "test-session"))
            .to_request(),
    )
    .await;
    let rows = rows.as_array().expect("authenticated invoice list");
    assert_eq!(rows.len(), 2);
    let cross = rows.iter().find(|r| r["id"] == "i").unwrap();
    assert_eq!(cross["is_crosschain"], true);
    assert_eq!(cross["received_zatoshis"], 10007000);
    assert_eq!(
        rows.iter().find(|r| r["id"] == "direct").unwrap()["is_crosschain"],
        false
    );
    assert!(cross.get("payload").is_none());
}

#[tokio::test]
async fn unused_reservation_preserves_and_resolves_direct_fee_obligation() {
    let (p, inv) = fixture().await;
    let mut body = quote_body(&inv, &input(), &asset(), false).unwrap();
    fees::apply(&mut body, Some("cipherscan.near"));
    reserve(&p, "i", body["deadline"].as_str().unwrap(), Some(&body))
        .await
        .unwrap();
    let paid = crate::invoices::record_payment(&p, "i", "direct", inv.price_zatoshis)
        .await
        .unwrap();
    crate::invoices::mark_detected(&p, "i", "direct", paid)
        .await
        .unwrap();
    crate::invoices::mark_confirmed(&p, "i", None, None)
        .await
        .unwrap();
    crate::billing::create_fee_entry(&p, "i", "m", 0.001, 0.01)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM deferred_payment_fees WHERE invoice_id='i'"
        )
        .fetch_one(&p)
        .await
        .unwrap(),
        "verifying"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM fee_ledger")
            .fetch_one(&p)
            .await
            .unwrap(),
        0
    );
    fees::resolve_direct(&p, "i").await.unwrap();
    fees::resolve_direct(&p, "i").await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT fee_amount_zatoshis FROM fee_ledger WHERE invoice_id='i'"
        )
        .fetch_one(&p)
        .await
        .unwrap(),
        100000
    );
}

#[tokio::test]
async fn quote_never_extends_direct_price_lock_and_recovery_accepts_only_delivery() {
    let (p, _) = fixture().await;
    let original =
        (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("UPDATE invoices SET expires_at=? WHERE id='i'")
        .bind(&original)
        .execute(&p)
        .await
        .unwrap();
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::days(3)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        crate::invoices::get_invoice(&p, "i")
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        original
    );
    sqlx::query("UPDATE crosschain_attempts SET price_expires_at='2000-01-01T00:00:00Z'")
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        crate::invoices::record_payment(&p, "i", "late-direct", 10000000)
            .await
            .unwrap(),
        0
    );
    sqlx::query("INSERT INTO crosschain_deliveries VALUES ('i','delivery')")
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        crate::invoices::record_payment(&p, "i", "delivery", 10000000)
            .await
            .unwrap(),
        10000000
    );
    assert_eq!(
        crate::invoices::record_payment(&p, "i", "delivery", 10000000)
            .await
            .unwrap(),
        10000000
    );
}

#[actix_web::test]
async fn public_invoice_cannot_create_adopt_or_lock_a_swap() {
    use actix_web::{test, App};
    let (p, _) = fixture().await;
    let mut config = crate::config::Config::from_env().unwrap();
    config.encryption_key = "11".repeat(32);
    let gateway = Gateway {
        enabled: true,
        fee_recipient: None,
        key: String::new(),
        base: "http://127.0.0.1:1".into(),
        client: reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),
        tokens: Mutex::new(None),
        eligibility: Mutex::new(HashMap::new()),
        eligibility_slots: Semaphore::new(4),
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(p.clone()))
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(gateway))
            .route("/{id}", web::post().to(create))
            .route("/{id}/send", web::post().to(wallet_send)),
    )
    .await;
    let body = json!({"asset_id":asset().asset_id,"refund_address":input().refund_address});
    for path in ["/i", "/i/send"] {
        let data = if path.ends_with("send") {
            json!({"action":"start"})
        } else {
            body.clone()
        };
        assert_eq!(
            test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(path)
                    .set_json(data)
                    .to_request()
            )
            .await
            .status(),
            403
        );
    }
    assert!(read_attempt(&p, "i", &config.encryption_key)
        .await
        .unwrap()
        .is_none());
    reserve(
        &p,
        "i",
        &(Utc::now() + Duration::minutes(30)).to_rfc3339(),
        None,
    )
    .await
    .unwrap();
    let payload = crate::crypto::encrypt(
        &json!({"asset":asset(),"refundAddress":"other"}).to_string(),
        &config.encryption_key,
    )
    .unwrap();
    sqlx::query("UPDATE crosschain_attempts SET payload=? WHERE invoice_id='i'")
        .bind(payload)
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/i")
                .insert_header(("X-Checkout-Token", security::token(&p, "i").await.unwrap()))
                .set_json(body)
                .to_request()
        )
        .await
        .status(),
        409
    );
}

#[test]
fn refund_addresses_require_network_checksums() {
    assert!(valid_refund("btc", "1BoatSLRHtKNngkdXEeobR76b53LETtpyT"));
    assert!(!valid_refund("btc", "1BoatSLRHtKNngkdXEeobR76b53LETtpyU"));
    assert!(valid_refund("tron", "TLa2f6VPqDgRE67v1736s7bJ8Ray5wYjU7"));
    assert!(!valid_refund("tron", "TLa2f6VPqDgRE67v1736s7bJ8Ray5wYjU8"));
    assert!(!valid_refund("sol", "11111111111111111111111111111111"));
    assert!(!valid_refund("sol", "z".repeat(44).as_str()));
    assert!(valid_refund(
        "eth",
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
    ));
    assert!(!valid_refund(
        "eth",
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD"
    ));
}

#[actix_web::test]
async fn private_swap_details_require_the_invoice_capability_for_every_state() {
    use actix_web::{test,App};
    let (p,_)=fixture().await;
    let mut config=crate::config::Config::from_env().unwrap();config.encryption_key="11".repeat(32);
    reserve(&p,"i",&(Utc::now()+Duration::minutes(10)).to_rfc3339(),None).await.unwrap();
    let payload=crate::crypto::encrypt(&json!({"asset":asset(),"refundAddress":"private-refund","depositAddress":"private-deposit","depositMemo":"private-memo","amountIn":"123","deadline":(Utc::now()+Duration::minutes(10)).to_rfc3339()}).to_string(),&config.encryption_key).unwrap();
    let gateway=Gateway{enabled:true,fee_recipient:None,key:String::new(),base:"http://127.0.0.1:1".into(),client:reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),tokens:Mutex::new(None),eligibility:Mutex::new(HashMap::new()),eligibility_slots:Semaphore::new(4)};
    let app=test::init_service(App::new().app_data(web::Data::new(p.clone())).app_data(web::Data::new(config)).app_data(web::Data::new(gateway)).route("/{id}",web::get().to(status))).await;
    let token=security::token(&p,"i").await.unwrap();
    for state in ["PENDING_DEPOSIT","PROCESSING","SUCCESS","REFUNDED","NEEDS_REVIEW"] {
        sqlx::query("UPDATE crosschain_attempts SET state=?,payload=?,sent_txid='private-tx',send_token='private-lock',last_checked=strftime('%s','now'),last_success=strftime('%s','now') WHERE invoice_id='i'").bind(state).bind(&payload).execute(&p).await.unwrap();
        for header in [None,Some("a".repeat(64))] {
            let mut req=test::TestRequest::get().uri("/i");if let Some(h)=header{req=req.insert_header(("X-Checkout-Token",h));}
            let public:Value=test::call_and_read_body_json(&app,req.to_request()).await;
            assert_eq!(public["canManage"],false);assert_eq!(public["asset"]["symbol"],"USDC");
            assert_eq!(public["settlementVerified"],false);
            assert!(!public.to_string().contains("private-"));assert!(public.get("quote").is_none());assert!(public.get("sentTxid").is_none());
        }
        let private:Value=test::call_and_read_body_json(&app,test::TestRequest::get().uri("/i").insert_header(("X-Checkout-Token",token.clone())).to_request()).await;
        assert_eq!(private["quote"]["refundAddress"],"private-refund");assert_eq!(private["canManage"],true);
        assert_eq!(private["settlementVerified"],false);
    }
    sqlx::query("INSERT INTO crosschain_settlements(invoice_id) VALUES ('i')").execute(&p).await.unwrap();
    for (invoice_status, verified) in [("detected", false), ("confirmed", true)] {
        sqlx::query("UPDATE invoices SET status=? WHERE id='i'").bind(invoice_status).execute(&p).await.unwrap();
        sqlx::query("UPDATE crosschain_attempts SET state='SUCCESS' WHERE invoice_id='i'").execute(&p).await.unwrap();
        for header in [None, Some(token.clone())] {
            let mut req=test::TestRequest::get().uri("/i");if let Some(h)=header{req=req.insert_header(("X-Checkout-Token",h));}
            let response:Value=test::call_and_read_body_json(&app,req.to_request()).await;
            assert_eq!(response["settlementVerified"],verified);
        }
    }
}

#[actix_web::test]
async fn disabling_crosschain_blocks_new_reservations_but_preserves_existing_deposits() {
    use actix_web::{test,App};
    let (p,_)=fixture().await;
    assert!(operations::enabled(&p,"i").await.unwrap());
    let deadline=(Utc::now()+Duration::minutes(10)).to_rfc3339();
    sqlx::query("UPDATE merchants SET crosschain_enabled=0 WHERE id='m'").execute(&p).await.unwrap();
    assert!(!reserve(&p,"i",&deadline,None).await.unwrap());
    let config=crate::config::Config::from_env().unwrap();
    let gateway=Gateway{enabled:true,fee_recipient:None,key:String::new(),base:"http://127.0.0.1:1".into(),client:reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),tokens:Mutex::new(Some((Instant::now(),vec![asset()]))),eligibility:Mutex::new(HashMap::new()),eligibility_slots:Semaphore::new(4)};
    assert!(prepare(&p,&gateway,"i",&input(),true).await.unwrap_err().to_string().contains("ZEC only"));
    let app=test::init_service(App::new().app_data(web::Data::new(p.clone())).app_data(web::Data::new(config)).app_data(web::Data::new(gateway)).route("/{id}",web::post().to(create)).route("/{id}/eligibility",web::get().to(eligibility))).await;
    let e:Value=test::call_and_read_body_json(&app,test::TestRequest::get().uri("/i/eligibility").to_request()).await;
    assert_eq!(e["enabled"],false);
    sqlx::query("UPDATE merchants SET crosschain_enabled=1 WHERE id='m'").execute(&p).await.unwrap();
    assert!(reserve(&p,"i",&deadline,None).await.unwrap());
    sqlx::query("UPDATE merchants SET crosschain_enabled=0 WHERE id='m'").execute(&p).await.unwrap();
    let a:Value=test::call_and_read_body_json(&app,test::TestRequest::post().uri("/i").insert_header(("X-Checkout-Token",security::token(&p,"i").await.unwrap())).set_json(json!({"asset_id":asset().asset_id,"refund_address":input().refund_address})).to_request()).await;
    assert_eq!(a["state"],"CREATING"); // no provider call or competing deposit
}

#[actix_web::test]
async fn interrupted_setup_is_reported_and_cannot_be_retried_as_a_new_quote() {
    use actix_web::{test,App};
    let (p,_)=fixture().await;
    let mut config=crate::config::Config::from_env().unwrap();config.admin_key=Some("test-admin".into());
    reserve(&p,"i",&(Utc::now()+Duration::minutes(10)).to_rfc3339(),None).await.unwrap();
    sqlx::query("UPDATE crosschain_attempts SET state='NEEDS_REVIEW' WHERE invoice_id='i'").execute(&p).await.unwrap();
    let issues=operations::issues(&p).await.unwrap();assert_eq!(issues[0]["reason"],"setup_review");
    let app=test::init_service(App::new().app_data(web::Data::new(p.clone())).app_data(web::Data::new(config)).route("/issues",web::get().to(operations::admin_issues)).route("/{id}",web::post().to(operations::retry))).await;
    assert_eq!(test::call_service(&app,test::TestRequest::get().uri("/issues").to_request()).await.status(),401);
    assert_eq!(test::call_service(&app,test::TestRequest::post().uri("/i").insert_header(("X-Admin-Key","test-admin")).to_request()).await.status(),409);
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT state FROM crosschain_attempts WHERE invoice_id='i'").fetch_one(&p).await.unwrap(),"NEEDS_REVIEW");
    sqlx::query("UPDATE crosschain_attempts SET payload='encrypted-existing-payload',last_checked=12345 WHERE invoice_id='i'").execute(&p).await.unwrap();
    assert_eq!(test::call_service(&app,test::TestRequest::post().uri("/i").insert_header(("X-Admin-Key","test-admin")).to_request()).await.status(),200);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT last_checked FROM crosschain_attempts WHERE invoice_id='i'").fetch_one(&p).await.unwrap(),0);
}

#[actix_web::test]
async fn status_outage_and_tampered_quotes_never_accept_payment_or_unlock_wallet() {
    use actix_web::{test,App,HttpServer};
    use std::sync::atomic::{AtomicU8,Ordering};
    let (p,inv)=fixture().await;
    let mut config=crate::config::Config::from_env().unwrap();config.encryption_key="11".repeat(32);
    let body=quote_body(&inv,&input(),&asset(),false).unwrap();
    let q=json!({"asset":asset(),"amountIn":"12345678","amountOut":"10000000","refundAddress":input().refund_address,"depositAddress":"0x2345678901234567890123456789012345678901","deadline":body["deadline"],"confidentiality":"basic"});
    let signed=signatures::signed(json!({"quoteRequest":body,"quote":{"amountIn":q["amountIn"],"amountOut":q["amountOut"],"depositAddress":q["depositAddress"],"deadline":q["deadline"]}}));
    let mode=web::Data::new(AtomicU8::new(0));let mm=mode.clone();
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let base=format!("http://{}",listener.local_addr().unwrap());
    let response=web::Data::new(signed);
    let server=HttpServer::new(move||App::new().app_data(mm.clone()).app_data(response.clone()).route("/status",web::get().to(|mode:web::Data<AtomicU8>,q:web::Data<Value>|async move{
        let m=mode.load(Ordering::SeqCst);if m==0{return HttpResponse::ServiceUnavailable().finish();}
        let mut quote=q.get_ref().clone();if m==1{quote["quote"]["depositAddress"]=json!("attacker");}
        if m==4{quote["quoteRequest"]["confidentiality"]=json!("public");}
        if m==5{quote["quoteRequest"].as_object_mut().unwrap().remove("confidentiality");}
        HttpResponse::Ok().json(json!({"status":if m==3{"REFUNDED"}else{"SUCCESS"},"quoteResponse":quote}))
    }))).listen(listener).unwrap().run();let handle=server.handle();actix_web::rt::spawn(server);
    reserve(&p,"i",body["deadline"].as_str().unwrap(),None).await.unwrap();
    let payload=crate::crypto::encrypt(&q.to_string(),&config.encryption_key).unwrap();
    sqlx::query("UPDATE crosschain_attempts SET state='PENDING_DEPOSIT',payload=?,send_token='keep-locked' WHERE invoice_id='i'").bind(payload).execute(&p).await.unwrap();
    let gateway=Gateway{enabled:true,fee_recipient:None,key:String::new(),base,client:reqwest::Client::new(), confirmation_client: reqwest::Client::new(), cipherscan_api_url: "http://127.0.0.1:1".into(),tokens:Mutex::new(None),eligibility:Mutex::new(HashMap::new()),eligibility_slots:Semaphore::new(4)};
    let app=test::init_service(App::new().app_data(web::Data::new(p.clone())).app_data(web::Data::new(config)).app_data(web::Data::new(gateway)).route("/{id}",web::get().to(status))).await;
    for m in 0..6 {
        mode.store(m,Ordering::SeqCst);
        sqlx::query("UPDATE crosschain_attempts SET state='PENDING_DEPOSIT',last_checked=0 WHERE invoice_id='i'").execute(&p).await.unwrap();
        let result:Value=test::call_and_read_body_json(&app,test::TestRequest::get().uri("/i").insert_header(("X-Checkout-Token",security::token(&p,"i").await.unwrap())).to_request()).await;
        assert_eq!(result["state"],if m==2{"SUCCESS"}else if m==3{"REFUNDED"}else{"PENDING_DEPOSIT"});
        assert_eq!(result["stale"],m<2 || m>=4);
        assert_eq!(result["sendStarted"],true);
        assert_eq!(crate::invoices::get_invoice(&p,"i").await.unwrap().unwrap().status,"pending");
    }
    handle.stop(true).await;
}

#[test]
#[ignore = "requires a real confidential dry quote supplied via SIGNED_QUOTE_FIXTURE"]
fn real_provider_confidential_quote() {
    let file = std::env::var("SIGNED_QUOTE_FIXTURE").unwrap();
    let response: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    signatures::verify(&response).unwrap();
    let mut body = response["quoteRequest"].clone();
    body["confidentiality"] = json!("basic");
    body["appFees"] = json!([{"recipient":"cipherscan.near","fee":125}]);
    assert_eq!(body["dry"], true);
    let quote = checked_quote(&response, &body, &asset(), true).unwrap();
    assert_eq!(quote["confidentiality"], "basic");
}
