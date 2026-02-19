use super::*;
use jsonwebtoken::{decode, DecodingKey, Validation};

#[test]
fn deterministic_room_name() {
    let room = room_name_for_voice_channel(GuildId(10), ChannelId(42));
    assert_eq!(room, "g:10:c:42");
}

#[test]
fn token_claims_contain_room_and_subject() {
    let cfg = LiveKitConfig {
        api_key: "devkey".into(),
        api_secret: "devsecret".into(),
        ttl_seconds: 60,
    };
    let token = mint_token(&cfg, UserId(7), "g:1:c:2", true, false).expect("token");

    let decoded = decode::<serde_json::Value>(
        &token,
        &DecodingKey::from_secret(cfg.api_secret.as_bytes()),
        &Validation::default(),
    )
    .expect("decode");

    assert_eq!(decoded.claims["iss"], "devkey");
    assert_eq!(decoded.claims["sub"], "user:7");
    assert_eq!(decoded.claims["video"]["room"], "g:1:c:2");
}
