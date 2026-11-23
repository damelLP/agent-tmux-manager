use super::push::{self, Endpoint};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, BigNumContext},
    ec::{EcKey, EcPoint},
    ecdsa::EcdsaSig,
    hash::{hash, MessageDigest},
    pkey::PKey,
    ssl::{SslAcceptor, SslMethod},
    x509::{extension::SubjectAlternativeName, X509NameBuilder, X509},
};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_util::sync::CancellationToken;

const URL: &str = "https://fcm.googleapis.com/wp/phone:opaque_token";

#[test]
fn fcm_endpoint_must_be_an_exact_https_origin_and_push_path() {
    assert!(Endpoint::parse(URL).is_ok());
    for rejected in [
        "http://fcm.googleapis.com/wp/phone",
        "https://fcm.googleapis.com.evil.invalid/wp/phone",
        "https://fcm.googleapis.com:443/wp/phone",
        "https://fcm.googleapis.com@elsewhere.invalid/wp/phone",
        "https://fcm.googleapis.com/wp/",
        "https://fcm.googleapis.com/wp/phone#fragment",
        "https://fcm.googleapis.com/wp/one,two",
        "https://fcm.googleapis.com/wp/x\"",
        "https://fcm.googleapis.com/other/token",
    ] {
        assert!(Endpoint::parse(rejected).is_err(), "{rejected}");
    }
}

fn tls_fixture() -> (tempfile::TempDir, SslAcceptor) {
    let dir = tempfile::tempdir().unwrap();
    let key = PKey::from_ec_key(push::new_key().unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "fcm.googleapis.com")
        .unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    cert.set_pubkey(&key).unwrap();
    let san = SubjectAlternativeName::new()
        .dns("fcm.googleapis.com")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();
    std::fs::write(dir.path().join("ca.pem"), cert.to_pem().unwrap()).unwrap();
    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    tls.set_certificate(&cert).unwrap();
    tls.set_private_key(&key).unwrap();
    (dir, tls.build())
}

fn capture_auth(request: &str, key: &push::Key) {
    let (t, public) = request
        .lines()
        .find_map(|l| l.strip_prefix("Authorization: vapid t="))
        .unwrap()
        .split_once(", k=")
        .unwrap();
    assert_eq!(B64.decode(public).unwrap().len(), 65);
    let point = EcPoint::from_bytes(
        key.group(),
        &B64.decode(public).unwrap(),
        &mut BigNumContext::new().unwrap(),
    )
    .unwrap();
    let verifying = EcKey::from_public_key(key.group(), &point).unwrap();
    let (signed, sig) = t.rsplit_once('.').unwrap();
    let (head, payload) = signed.split_once('.').unwrap();
    let head: Value = serde_json::from_slice(&B64.decode(head).unwrap()).unwrap();
    let payload: Value = serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap();
    assert_eq!(head["alg"], "ES256");
    assert_eq!(
        payload,
        serde_json::json!({"aud":"https://fcm.googleapis.com","exp":1600})
    );
    let raw = B64.decode(sig).unwrap();
    assert_eq!(raw.len(), 64);
    let sig = EcdsaSig::from_private_components(
        BigNum::from_slice(&raw[..32]).unwrap(),
        BigNum::from_slice(&raw[32..]).unwrap(),
    )
    .unwrap();
    assert!(sig
        .verify(
            &hash(MessageDigest::sha256(), signed.as_bytes()).unwrap(),
            &verifying
        )
        .unwrap());
}

#[tokio::test]
async fn curl_sends_only_empty_web_push_and_reaps_when_cancelled() {
    let (dir, tls) = tls_fixture();
    for cancel in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (noticed, observed) = oneshot::channel();
        let worker = tokio::spawn({
            let tls = tls.clone();
            async move {
                let socket = listener.accept().await.unwrap().0.into_std().unwrap();
                socket.set_nonblocking(false).unwrap();
                tokio::task::spawn_blocking(move || {
                socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
                let mut stream = BufReader::new(tls.accept(socket).unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(stream.read_line(&mut line).unwrap(), 0);
                    request.push_str(&line);
                    if line == "\r\n" { break; }
                }
                assert!(stream.buffer().is_empty());
                noticed.send(request).unwrap();
                if cancel {
                    // Curl is held at a real TLS request until the test cancels it.
                    let mut byte = [0];
                    match stream.read(&mut byte) {
                        Ok(0) => {},
                        Err(e) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset) => {},
                        other => panic!("curl did not hang up: {other:?}"),
                    }
                } else {
                    stream.get_mut().write_all(b"HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
                }).await.unwrap();
            }
        });
        let key = push::new_key().unwrap();
        let config = push::request(&key, &Endpoint::parse(URL).unwrap(), 1000).unwrap();
        let mut curl = push::curl();
        curl.args([
            "--connect-to",
            &format!("fcm.googleapis.com:443:127.0.0.1:{port}"),
        ])
        .arg("--cacert")
        .arg(dir.path().join("ca.pem"));
        let stop = CancellationToken::new();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(push::run(curl, config, &stop), async {
                let request = observed.await.unwrap();
                assert!(request.starts_with("POST /wp/phone:opaque_token HTTP/"));
                assert!(request.contains("\r\nContent-Length: 0\r\n"));
                assert!(request.contains("\r\nTopic: atm-chat\r\n"));
                assert!(request.contains("\r\nTTL: 600\r\n"));
                assert!(request.contains("\r\nUrgency: high\r\n"));
                capture_auth(&request, &key);
                if cancel {
                    stop.cancel();
                }
            })
        })
        .await
        .unwrap();
        assert_eq!(result.ok(), (!cancel).then_some(410));
        worker.await.unwrap();
    }
}
