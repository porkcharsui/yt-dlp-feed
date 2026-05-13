use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use base64::Engine;
use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use http::{Request, Response, StatusCode};
use tower::{Layer, Service};

use crate::config::AuthConfig;

#[derive(Debug, Clone)]
pub struct AuthLayer {
    config: AuthConfig,
}

#[derive(Debug, Clone)]
pub struct AuthService<S> {
    inner: S,
    config: AuthConfig,
}

impl AuthLayer {
    pub fn new(config: AuthConfig) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            config: self.config.clone(),
        }
    }
}

impl<S> Service<Request<Body>> for AuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if !self.config.enabled || request_is_authorized(&self.config, &req) {
            let mut inner = self.inner.clone();
            return Box::pin(async move { inner.call(req).await });
        }

        Box::pin(async move {
            Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(WWW_AUTHENTICATE, "Basic realm=\"yt-dlp-rss\"")
                .body(Body::from("authentication required"))
                .expect("valid unauthorized response"))
        })
    }
}

pub fn request_is_authorized(config: &AuthConfig, req: &Request<Body>) -> bool {
    let Some(expected_user) = config.username.as_deref() else {
        return false;
    };
    let Some(expected_password) = config.password.as_deref() else {
        return false;
    };
    let Some(header) = req.headers().get(AUTHORIZATION) else {
        return false;
    };
    let Ok(header) = header.to_str() else {
        return false;
    };
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(credentials) = String::from_utf8(decoded) else {
        return false;
    };
    credentials == format!("{expected_user}:{expected_password}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;

    #[test]
    fn disabled_auth_does_not_require_credentials() {
        let config = AuthConfig::default();
        let request = Request::builder().body(Body::empty()).unwrap();

        assert!(!request_is_authorized(&config, &request));
        assert!(!config.enabled);
    }

    #[test]
    fn validates_basic_auth_credentials() {
        let config = AuthConfig {
            enabled: true,
            username: Some("derek".to_string()),
            password: Some("secret".to_string()),
        };
        let token = base64::engine::general_purpose::STANDARD.encode("derek:secret");
        let request = Request::builder()
            .header(AUTHORIZATION, format!("Basic {token}"))
            .body(Body::empty())
            .unwrap();

        assert!(request_is_authorized(&config, &request));
    }
}
