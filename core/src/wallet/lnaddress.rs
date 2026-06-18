//! Lightning addresses (LUD-16) + LNURL-pay (LUD-06) resolution, and the
//! M-Pesa cash-out address builder.
//!
//! A **Lightning address** looks like an email — `user@domain` — and resolves
//! to a BOLT-11 invoice in two GETs:
//!
//! 1. `GET https://<domain>/.well-known/lnurlp/<user>` → the *pay params*
//!    (a `callback` URL plus `minSendable`/`maxSendable` in msats).
//! 2. `GET <callback>?amount=<msats>` → `{ "pr": "<bolt11>" }`.
//!
//! The **M-Pesa payout** is exactly this flow against the `bitcoin.co.ke`
//! Lightning-address service: paying `<phone>@bitcoin.co.ke` makes that service
//! convert the received sats to KES and push them to the phone's M-Pesa wallet.
//!
//! URL building and JSON parsing are pure and host-tested; [`resolve`] is the
//! only part that touches the network, over the same minimal rustls/**ring**
//! [`https_get_str`](crate::geo::http::https_get_str) the map/relay layers use.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::geo::http::https_get_str;
use crate::wallet::Amount;

/// The `bitcoin.co.ke` Lightning-address host that performs the M-Pesa payout.
pub const MPESA_DOMAIN: &str = "bitcoin.co.ke";

/// A LUD-16 Lightning address, `user@domain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightningAddress {
    /// The local part (before `@`) — e.g. a username, or an M-Pesa phone number.
    pub user: String,
    /// The domain (after `@`).
    pub domain: String,
}

impl LightningAddress {
    /// The `/.well-known/lnurlp/<user>` request path for the first GET.
    pub fn lnurlp_path(&self) -> String {
        format!("/.well-known/lnurlp/{}", self.user)
    }
}

impl std::fmt::Display for LightningAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.domain)
    }
}

impl std::str::FromStr for LightningAddress {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (user, domain) = s
            .split_once('@')
            .ok_or_else(|| Error::Wallet(format!("not a lightning address: {s:?}")))?;
        if user.is_empty() || domain.is_empty() || domain.contains('@') {
            return Err(Error::Wallet(format!("not a lightning address: {s:?}")));
        }
        // Domains never contain a slash or whitespace; reject obvious junk early.
        if domain.contains('/') || domain.split_whitespace().count() != 1 {
            return Err(Error::Wallet(format!("bad lightning-address domain: {domain:?}")));
        }
        Ok(LightningAddress {
            user: user.to_string(),
            domain: domain.to_string(),
        })
    }
}

/// Build the `<phone>@bitcoin.co.ke` M-Pesa payout address, normalizing the
/// phone number to its international (`2547…`) form first.
pub fn mpesa_address(phone: &str) -> Result<LightningAddress> {
    Ok(LightningAddress {
        user: normalize_phone(phone)?,
        domain: MPESA_DOMAIN.to_string(),
    })
}

/// Normalize a Kenyan phone number to bare international digits (`2547XXXXXXXX`).
///
/// Accepts the common shapes — `0712 345 678`, `+254712345678`,
/// `254712345678`, `712345678` — and strips spaces/dashes/parentheses. A
/// leading `0` (national trunk) becomes the `254` country code.
pub fn normalize_phone(phone: &str) -> Result<String> {
    // Keep digits only (drop +, spaces, dashes, parentheses, etc.).
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 9 {
        return Err(Error::Wallet(format!("not a phone number: {phone:?}")));
    }
    let normalized = if let Some(rest) = digits.strip_prefix("254") {
        format!("254{rest}")
    } else if let Some(rest) = digits.strip_prefix('0') {
        // National form 07.. / 01.. → 2547.. / 2541..
        format!("254{rest}")
    } else if digits.len() == 9 {
        // Bare subscriber number (7.. / 1..) → prepend the country code.
        format!("254{digits}")
    } else {
        digits
    };
    Ok(normalized)
}

// ---- LNURL-pay params (step 1 response) -----------------------------------

/// The LNURL-pay parameters returned by the `.well-known/lnurlp/<user>` GET.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PayParams {
    /// The second-GET URL that mints the invoice.
    pub callback: String,
    /// Minimum payable amount, in **millisatoshis**.
    #[serde(rename = "minSendable")]
    pub min_sendable: u64,
    /// Maximum payable amount, in **millisatoshis**.
    #[serde(rename = "maxSendable")]
    pub max_sendable: u64,
    /// Must be `"payRequest"` for a pay endpoint.
    #[serde(default)]
    pub tag: String,
    /// Some endpoints surface an error here instead of HTTP status.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Parse + validate the step-1 pay params.
pub fn parse_pay_params(body: &str) -> Result<PayParams> {
    let params: PayParams = serde_json::from_str(body)
        .map_err(|e| Error::Wallet(format!("bad LNURL-pay params: {e}")))?;
    if params.status.as_deref() == Some("ERROR") {
        return Err(Error::Wallet(format!(
            "LNURL-pay error: {}",
            params.reason.as_deref().unwrap_or("unknown")
        )));
    }
    if !params.tag.is_empty() && params.tag != "payRequest" {
        return Err(Error::Wallet(format!(
            "not an LNURL-pay endpoint (tag={:?})",
            params.tag
        )));
    }
    if params.min_sendable == 0 || params.max_sendable < params.min_sendable {
        return Err(Error::Wallet("LNURL-pay sendable range is invalid".into()));
    }
    Ok(params)
}

// ---- callback (step 2) ----------------------------------------------------

/// The step-2 callback response carrying the invoice.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct PayResponse {
    #[serde(default)]
    pr: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Parse the step-2 response, returning the BOLT-11 invoice string.
pub fn parse_invoice(body: &str) -> Result<String> {
    let resp: PayResponse = serde_json::from_str(body)
        .map_err(|e| Error::Wallet(format!("bad LNURL callback response: {e}")))?;
    if resp.status.as_deref() == Some("ERROR") {
        return Err(Error::Wallet(format!(
            "LNURL callback error: {}",
            resp.reason.as_deref().unwrap_or("unknown")
        )));
    }
    resp.pr
        .filter(|pr| !pr.is_empty())
        .ok_or_else(|| Error::Wallet("LNURL callback returned no invoice".into()))
}

/// Split an `https://host/path?query` URL into `(host, "/path?query")` for
/// [`https_get_str`]. Only `https`/`http` are accepted.
pub fn split_url(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| Error::Wallet(format!("callback is not http(s): {url:?}")))?;
    match rest.find('/') {
        Some(i) => {
            let host = &rest[..i];
            let path = &rest[i..];
            if host.is_empty() {
                return Err(Error::Wallet(format!("callback has no host: {url:?}")));
            }
            Ok((host.to_string(), path.to_string()))
        }
        // No path component: the whole remainder is the host, path is root.
        None if !rest.is_empty() => Ok((rest.to_string(), "/".to_string())),
        _ => Err(Error::Wallet(format!("callback has no host: {url:?}"))),
    }
}

/// Build the step-2 request target: split the callback URL and append the
/// `amount` (in msats) as a query parameter, preserving any existing query.
pub fn callback_target(callback: &str, amount: Amount) -> Result<(String, String)> {
    let (host, path) = split_url(callback)?;
    let sep = if path.contains('?') { '&' } else { '?' };
    let path = format!("{path}{sep}amount={}", amount.msats());
    Ok((host, path))
}

// ---- the full async resolution --------------------------------------------

/// Resolve a Lightning `address` to a BOLT-11 invoice for `amount`, performing
/// the two LUD-16/LUD-06 GETs. Validates that `amount` is within the endpoint's
/// sendable range. Network/parse/validation failures → [`Error::Wallet`].
pub async fn resolve(address: &LightningAddress, amount: Amount) -> Result<String> {
    // Step 1: fetch + validate the pay params.
    let body = https_get_str(&address.domain, &address.lnurlp_path()).await?;
    let params = parse_pay_params(&body)?;

    let msats = amount.msats();
    if msats < params.min_sendable || msats > params.max_sendable {
        return Err(Error::Wallet(format!(
            "{} is outside the payable range {}–{} sat for {address}",
            amount,
            params.min_sendable / 1000,
            params.max_sendable / 1000,
        )));
    }

    // Step 2: hit the callback for the invoice.
    let (host, path) = callback_target(&params.callback, amount)?;
    let body = https_get_str(&host, &path).await?;
    parse_invoice(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_addresses() {
        let a: LightningAddress = "alice@walletofsatoshi.com".parse().unwrap();
        assert_eq!(a.user, "alice");
        assert_eq!(a.domain, "walletofsatoshi.com");
        assert_eq!(a.to_string(), "alice@walletofsatoshi.com");
        assert_eq!(a.lnurlp_path(), "/.well-known/lnurlp/alice");
    }

    #[test]
    fn rejects_non_addresses() {
        assert!("nope".parse::<LightningAddress>().is_err());
        assert!("@domain.com".parse::<LightningAddress>().is_err());
        assert!("user@".parse::<LightningAddress>().is_err());
        assert!("a@b@c".parse::<LightningAddress>().is_err());
        assert!("user@dom ain".parse::<LightningAddress>().is_err());
    }

    #[test]
    fn normalizes_kenyan_phone_numbers() {
        assert_eq!(normalize_phone("0712 345 678").unwrap(), "254712345678");
        assert_eq!(normalize_phone("+254712345678").unwrap(), "254712345678");
        assert_eq!(normalize_phone("254712345678").unwrap(), "254712345678");
        assert_eq!(normalize_phone("0112345678").unwrap(), "254112345678");
        assert_eq!(normalize_phone("712345678").unwrap(), "254712345678");
        assert_eq!(normalize_phone("(0712)-345-678").unwrap(), "254712345678");
        assert!(normalize_phone("123").is_err());
        assert!(normalize_phone("not a phone").is_err());
    }

    #[test]
    fn builds_mpesa_address() {
        let a = mpesa_address("0712 345 678").unwrap();
        assert_eq!(a.to_string(), "254712345678@bitcoin.co.ke");
        assert_eq!(a.lnurlp_path(), "/.well-known/lnurlp/254712345678");
    }

    #[test]
    fn parses_pay_params() {
        let body = r#"{
            "callback":"https://bitcoin.co.ke/lnurlp/api/v1/lnurl/cb/xyz",
            "minSendable":1000,"maxSendable":100000000,
            "metadata":"[[\"text/plain\",\"pay\"]]","tag":"payRequest"
        }"#;
        let p = parse_pay_params(body).unwrap();
        assert_eq!(p.callback, "https://bitcoin.co.ke/lnurlp/api/v1/lnurl/cb/xyz");
        assert_eq!(p.min_sendable, 1000);
        assert_eq!(p.max_sendable, 100_000_000);
    }

    #[test]
    fn rejects_error_and_non_pay_params() {
        assert!(parse_pay_params(r#"{"status":"ERROR","reason":"no such user"}"#).is_err());
        assert!(parse_pay_params(
            r#"{"callback":"https://x/y","minSendable":1000,"maxSendable":2000,"tag":"withdrawRequest"}"#
        )
        .is_err());
        // max < min is invalid.
        assert!(parse_pay_params(
            r#"{"callback":"https://x/y","minSendable":5000,"maxSendable":1000,"tag":"payRequest"}"#
        )
        .is_err());
    }

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_url("https://bitcoin.co.ke/lnurlp/cb/xyz").unwrap(),
            ("bitcoin.co.ke".into(), "/lnurlp/cb/xyz".into())
        );
        assert_eq!(
            split_url("https://host.example").unwrap(),
            ("host.example".into(), "/".into())
        );
        assert!(split_url("ftp://nope/x").is_err());
    }

    #[test]
    fn builds_callback_target_with_amount() {
        // No existing query → `?amount`.
        let (host, path) =
            callback_target("https://bitcoin.co.ke/cb/xyz", Amount::from_sats(500)).unwrap();
        assert_eq!(host, "bitcoin.co.ke");
        assert_eq!(path, "/cb/xyz?amount=500000");
        // Existing query → `&amount`.
        let (_h, path) =
            callback_target("https://host/cb?k=v", Amount::from_sats(1)).unwrap();
        assert_eq!(path, "/cb?k=v&amount=1000");
    }

    #[test]
    fn parses_callback_invoice() {
        assert_eq!(
            parse_invoice(r#"{"pr":"lnbc500u1pexample","routes":[]}"#).unwrap(),
            "lnbc500u1pexample"
        );
        assert!(parse_invoice(r#"{"status":"ERROR","reason":"expired"}"#).is_err());
        assert!(parse_invoice(r#"{"routes":[]}"#).is_err());
    }
}
