use super::{account::AccountTokens, Error};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub(crate) struct Claims {
    pub aud: String,
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Check {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims: Option<Claims>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broker_status: Option<u16>,
}
impl Check {
    fn refusal(error: Error) -> Self {
        Self {
            status: match error {
                Error::AccountAuthRequired => "auth_required",
                Error::AccountNotFound => "not_found",
                Error::AccountTransient(_) => "transient",
                Error::AccountPermanent => "permanent",
                _ => "invalid",
            }
            .into(),
            retry_after_ms: error.retry_after_ms(),
            claims: None,
            broker_status: None,
        }
    }
}

// This check deliberately has no access to job storage or the hosted switch.
// Only the vault mints the bearer; neither it nor broker response text is returned.
pub(crate) async fn check(tokens: Option<&Arc<AccountTokens>>, config: &crate::Config) -> Check {
    let token = match tokens {
        Some(tokens) => tokens.bearer().await,
        None => Err(Error::AccountNotFound),
    };
    let token = match token {
        Ok(token) => token,
        Err(error) => return Check::refusal(error),
    };
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<Claims>(&payload).ok());
    let Some(claims) = claims.filter(|c| {
        c.aud == "cortexkit-runners" && !c.sub.is_empty() && c.iat < c.exp && c.exp > super::now_s()
    }) else {
        return Check::refusal(Error::AccountInvalid);
    };
    let broker_status = if let Some(url) = &config.broker_url {
        match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(super::account::BROKER_TIMEOUT)
            .build()
        {
            Ok(client) => client
                .post(format!("{}/v1/leases", url.trim_end_matches('/')))
                .bearer_auth(&token)
                .json(&serde_json::json!({}))
                .send()
                .await
                .ok()
                .map(|reply| reply.status().as_u16()),
            Err(_) => None,
        }
    } else {
        None
    };
    Check {
        status: "ok".into(),
        retry_after_ms: None,
        claims: Some(claims),
        broker_status,
    }
}

#[cfg(test)]
mod tests;
