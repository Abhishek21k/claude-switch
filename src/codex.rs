//! Offline identity extraction from a Codex home. Token bytes never leave this module.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub plan_type: Option<String>,
}

pub fn identity_from_auth(bytes: &[u8]) -> Option<Identity> {
    let auth: Value = serde_json::from_slice(bytes).ok()?;
    let token = auth.get("tokens")?.get("id_token")?.as_str()?;
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let claims: Value = serde_json::from_slice(&decode_base64url(payload)?).ok()?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.trim().is_empty())
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")?
                .get("email")?
                .as_str()
                .filter(|email| !email.trim().is_empty())
        })?
        .trim();
    if email.is_empty() {
        return None;
    }
    let plan_type = claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .filter(|plan| !plan.trim().is_empty())
        .map(str::to_string);
    Some(Identity {
        email: email.to_string(),
        plan_type,
    })
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let unpadded = bytes
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(bytes.len());
    let padding = bytes.len() - unpadded;
    if padding > 2 || bytes[unpadded..].iter().any(|byte| *byte != b'=') {
        return None;
    }
    if unpadded % 4 == 1 || (padding > 0 && !bytes.len().is_multiple_of(4)) {
        return None;
    }
    let mut output = Vec::with_capacity(unpadded * 3 / 4);
    let mut bits = 0u32;
    let mut count = 0u8;
    for &byte in &bytes[..unpadded] {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(digit);
        count += 6;
        if count >= 8 {
            count -= 8;
            output.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    if bits != 0 {
        return None;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(data: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut encoded = String::new();
        for chunk in data.chunks(3) {
            let number = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            encoded.push(ALPHABET[((number >> 18) & 63) as usize] as char);
            encoded.push(ALPHABET[((number >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(ALPHABET[((number >> 6) & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                encoded.push(ALPHABET[(number & 63) as usize] as char);
            }
        }
        encoded
    }

    #[test]
    fn identity_reads_synthetic_claims_without_padding() {
        // Known-bad: treating an unpadded JWT payload as invalid or missing its plan claim.
        let claims = br#"{"email":"user@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}} "#;
        assert_ne!(encode(claims).len() % 4, 0);
        let auth = format!(
            r#"{{"tokens":{{"id_token":"header.{}.signature"}}}}"#,
            encode(claims)
        );
        assert_eq!(
            identity_from_auth(auth.as_bytes()),
            Some(Identity {
                email: "user@example.com".into(),
                plan_type: Some("plus".into()),
            })
        );
    }

    #[test]
    fn identity_uses_profile_email_fallback() {
        // Known-bad: only the top-level email is accepted.
        let claims = br#"{"https://api.openai.com/profile":{"email":"fallback@example.com"}}"#;
        let auth = format!(r#"{{"tokens":{{"id_token":"h.{}.s"}}}}"#, encode(claims));
        assert_eq!(
            identity_from_auth(auth.as_bytes()).unwrap().email,
            "fallback@example.com"
        );
    }

    #[test]
    fn identity_falls_back_when_top_level_email_is_empty() {
        // Known-bad: an empty top-level email masks a populated profile claim.
        let claims =
            br#"{"email":" ","https://api.openai.com/profile":{"email":"fallback@example.com"}}"#;
        let auth = format!(r#"{{"tokens":{{"id_token":"h.{}.s"}}}}"#, encode(claims));
        assert_eq!(
            identity_from_auth(auth.as_bytes()).unwrap().email,
            "fallback@example.com"
        );
    }

    #[test]
    fn identity_rejects_missing_token() {
        // Known-bad: missing token data is treated as an account.
        assert_eq!(identity_from_auth(br#"{}"#), None);
    }

    #[test]
    fn identity_rejects_bad_segment_count() {
        // Known-bad: indexing JWT segments panics when the signature is missing.
        assert_eq!(
            identity_from_auth(br#"{"tokens":{"id_token":"bad"}}"#),
            None
        );
    }

    #[test]
    fn identity_rejects_empty_jwt_segments() {
        // Known-bad: a token with an empty header or signature is treated as a valid JWT.
        let payload = encode(br#"{"email":"user@example.com"}"#);
        for token in [format!(".{payload}.s"), format!("h.{payload}.")] {
            let auth = format!(r#"{{"tokens":{{"id_token":"{token}"}}}}"#);
            assert_eq!(identity_from_auth(auth.as_bytes()), None);
        }
    }

    #[test]
    fn identity_rejects_non_json_payload() {
        // Known-bad: a decoded non-JSON payload is accepted or panics during claim access.
        assert_eq!(
            identity_from_auth(br#"{"tokens":{"id_token":"h.bm90anNvbg.s"}}"#),
            None
        );
    }

    #[test]
    fn identity_rejects_invalid_base64url() {
        // Known-bad: invalid base64url characters silently decode into an identity.
        assert_eq!(
            identity_from_auth(br#"{"tokens":{"id_token":"h.@@.s"}}"#),
            None
        );
    }
}
