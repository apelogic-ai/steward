use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::EncodePrivateKey;
use rand_core::OsRng;
use steward_apiserver::browser_hop1_attestation::BrowserHop1AttestationConfig;

#[test]
fn configured_es256_signer_must_cryptographically_match_its_published_jwks()
-> Result<(), Box<dyn std::error::Error>> {
    let private = SecretKey::random(&mut OsRng);
    let public = private.public_key().to_encoded_point(false);
    let x = URL_SAFE_NO_PAD.encode(public.x().ok_or("missing P-256 x coordinate")?);
    let y = URL_SAFE_NO_PAD.encode(public.y().ok_or("missing P-256 y coordinate")?);
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "EC", "use": "sig", "alg": "ES256", "kid": "browser-hop1-current",
            "crv": "P-256", "x": x, "y": y
        }]
    });
    let private_der = private.to_pkcs8_der()?;

    let config = BrowserHop1AttestationConfig::from_pkcs8_der_and_jwks(
        "https://steward.example.test".to_owned(),
        "identity-browser-hop1".to_owned(),
        "browser-hop1-current".to_owned(),
        private_der.as_bytes(),
        &serde_json::to_string(&jwks)?,
    )
    .map_err(|error| format!("a matching signer/JWKS pair must be accepted: {error:?}"))?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(config.public_jwks())?,
        jwks,
        "the published JWKS must be derived from the configured private signer"
    );
    Ok(())
}
