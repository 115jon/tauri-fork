// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  borrow::Cow,
  collections::HashMap,
  sync::{Arc, Mutex},
};

use http::{
  Request, Response as HttpResponse, StatusCode, header::CONTENT_TYPE,
};
use tauri_utils::config::HeaderAddition;

use crate::{
  Runtime,
  manager::{AppManager, webview::PROXY_DEV_SERVER},
  webview::{UriSchemeProtocolHandler, WebResourceRequestHandler},
};

#[derive(Clone)]
struct CachedResponse {
  status: http::StatusCode,
  headers: http::HeaderMap,
  body: bytes::Bytes,
}

pub fn get<R: Runtime>(
  #[allow(unused_variables)] manager: Arc<AppManager<R>>,
  window_origin: &str,
  web_resource_request_handler: Option<Box<WebResourceRequestHandler>>,
) -> UriSchemeProtocolHandler {
  let url = {
    let mut url = manager
      .get_app_url(window_origin.starts_with("https"))
      .as_str()
      .to_string();
    if url.ends_with('/') {
      url.pop();
    }
    url
  };

  let window_origin = window_origin.to_string();

  let response_cache = Arc::new(Mutex::new(HashMap::new()));

  Box::new(move |_, request, responder| {
    match get_response(
      request,
      &manager,
      &window_origin,
      web_resource_request_handler.as_deref(),
      (&url, &response_cache),
    ) {
      Ok(response) => responder.respond(response),
      Err(e) => responder.respond(
        HttpResponse::builder()
          .status(StatusCode::INTERNAL_SERVER_ERROR)
          .header(CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())
          .header("Access-Control-Allow-Origin", &window_origin)
          .body(e.to_string().into_bytes())
          .unwrap(),
      ),
    }
  })
}

fn get_response<R: Runtime>(
  #[allow(unused_mut)] mut request: Request<Vec<u8>>,
  #[allow(unused_variables)] manager: &AppManager<R>,
  window_origin: &str,
  web_resource_request_handler: Option<&WebResourceRequestHandler>,
  (url, response_cache): (&str, &Arc<Mutex<HashMap<String, CachedResponse>>>),
) -> Result<HttpResponse<Cow<'static, [u8]>>, Box<dyn std::error::Error>> {
  let proxy_dev_server =
    PROXY_DEV_SERVER && manager.assets.iter().next().is_none();
  // use the entire URI as we are going to proxy the request
  let path = if proxy_dev_server {
    request.uri().to_string()
  } else {
    // ignore query string and fragment
    request
      .uri()
      .to_string()
      .split(&['?', '#'][..])
      .next()
      .unwrap()
      .into()
  };

  let path = path
    .strip_prefix(window_origin)
    // wry always sends us <scheme>://localhost format for custom protocols
    // even when it is actually http://<scheme>.localhost
    .or_else(|| path.strip_prefix("tauri://localhost"))
    .map(|p| p.to_string())
    .unwrap_or_default();

  let mut builder = HttpResponse::builder()
    .add_configured_headers(
      request.uri().path(),
      manager.config.app.security.headers.as_ref(),
    )
    .header("Access-Control-Allow-Origin", window_origin);

  let mut response = if proxy_dev_server {
    let decoded_path = percent_encoding::percent_decode(path.as_bytes())
      .decode_utf8_lossy()
      .to_string();
    let url = format!(
      "{}/{}",
      url.trim_end_matches('/'),
      decoded_path.trim_start_matches('/')
    );

    #[cfg(feature = "rustls-tls")]
    if rustls::crypto::CryptoProvider::get_default().is_none() {
      let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[allow(unused_mut)]
    let mut client = reqwest::ClientBuilder::new();

    if url.starts_with("https://") {
      // we can't load env vars at runtime, gotta embed them in the lib
      if let Some(cert_pem) = option_env!("TAURI_DEV_ROOT_CERTIFICATE") {
        #[cfg(any(
          feature = "native-tls",
          feature = "native-tls-vendored",
          feature = "rustls-tls"
        ))]
        {
          log::info!("adding dev server root certificate");
          let certificate = reqwest::Certificate::from_pem(cert_pem.as_bytes())
            .expect("failed to parse TAURI_DEV_ROOT_CERTIFICATE");
          client = client.tls_certs_merge([certificate]);
        }

        #[cfg(not(any(
          feature = "native-tls",
          feature = "native-tls-vendored",
          feature = "rustls-tls"
        )))]
        {
          let _cert_pem = cert_pem;
          log::warn!(
            "the dev root-certificate-path option was provided, but you must enable one of the following Tauri features in Cargo.toml: native-tls, native-tls-vendored, rustls-tls"
          );
        }
      } else {
        log::warn!(
          "loading HTTPS URL; you might need to provide a certificate via the `dev --root-certificate-path` option. You must enable one of the following Tauri features in Cargo.toml: native-tls, native-tls-vendored, rustls-tls"
        );
      }
    }

    // Disable automatic decompression so that Content-Length and
    // Content-Encoding headers from the upstream remain consistent
    // with the body bytes. Without this, reqwest silently decompresses
    // gzipped responses but forwards the original (compressed)
    // Content-Length header, causing a length mismatch.
    let mut proxy_builder = client
      .no_gzip()
      .no_brotli()
      .no_deflate()
      .build()
      .unwrap()
      .request(request.method().clone(), &url);

    // Forward all request headers (including Range) to the dev server
    for (name, value) in request.headers() {
      proxy_builder = proxy_builder.header(name, value);
    }

    // Forward request body (only relevant for POST/PUT, but correct for all methods)
    proxy_builder = proxy_builder.body(std::mem::take(request.body_mut()));

    match crate::async_runtime::safe_block_on(proxy_builder.send()) {
      Ok(r) => {
        let status = r.status();
        let upstream_headers = r.headers().clone();
        let body = crate::async_runtime::safe_block_on(r.bytes())?;

        // Only cache 200 OK responses. Never cache:
        // - 206 Partial Content: cache key doesn't include Range header,
        //   so a cached partial response would be served for different
        //   byte ranges, causing ERR_REQUEST_RANGE_NOT_SATISFIABLE.
        // - 304 Not Modified: only use cached entry if one exists.
        if status == http::StatusCode::NOT_MODIFIED {
          let response_cache_ = response_cache.lock().unwrap();
          if let Some(cached) = response_cache_.get(&url) {
            for (name, value) in &cached.headers {
              builder = builder.header(name, value);
            }
            return Ok(
              builder
                .status(cached.status)
                .body(cached.body.to_vec().into())?,
            );
          }
          // No cached entry for 304 — fall through and return as-is
        } else if status == http::StatusCode::OK {
          let mut response_cache_ = response_cache.lock().unwrap();
          response_cache_.insert(
            url.clone(),
            CachedResponse {
              status,
              headers: upstream_headers.clone(),
              body: body.clone(),
            },
          );
        }

        // CEF's media stack on http://tauri.localhost completely ignores
        // Cache-Control headers (including no-store) for 206 Partial Content
        // responses. It caches the 206 entry and then fails subsequent Range
        // requests for different byte ranges with ERR_REQUEST_RANGE_NOT_SATISFIABLE.
        //
        // Fix: when the 206 response contains the full resource (the common case
        // since our proxy fully buffers), rewrite it to 200 OK and drop the
        // Content-Range header. CEF's cache then stores the complete resource
        // and can serve any subsequent Range request from cache.
        let is_full_resource_206 = status == http::StatusCode::PARTIAL_CONTENT
          && upstream_headers
            .get(http::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|cr| {
              if let Some(rest) = cr.strip_prefix("bytes ") {
                if let Some((range_part, total_str)) = rest.split_once('/') {
                  let mut parts = range_part.splitn(2, '-');
                  if let (Some(s), Some(e)) = (parts.next(), parts.next()) {
                    if let (Ok(start), Ok(end), Ok(total)) = (
                      s.parse::<u64>(),
                      e.parse::<u64>(),
                      total_str.parse::<u64>(),
                    ) {
                      return start == 0 && end == total - 1;
                    }
                  }
                }
              }
              false
            })
            .unwrap_or(false);

        let final_status = if is_full_resource_206 {
          http::StatusCode::OK
        } else {
          status
        };

        // Forward upstream headers, skipping headers we manage ourselves
        for (name, value) in &upstream_headers {
          if name == http::header::CONTENT_LENGTH {
            continue;
          }
          // When converting 206→200, drop Content-Range (no longer partial)
          // and Cache-Control/Pragma (we want CEF to cache the full resource)
          if is_full_resource_206
            && (name == http::header::CONTENT_RANGE
              || name == http::header::CACHE_CONTROL
              || name == http::header::PRAGMA)
          {
            continue;
          }
          // For genuinely partial 206, strip any caching headers to prevent
          // CEF from caching a partial response
          if status == http::StatusCode::PARTIAL_CONTENT
            && !is_full_resource_206
            && (name == http::header::CACHE_CONTROL
              || name == http::header::PRAGMA)
          {
            continue;
          }
          builder = builder.header(name, value);
        }

        // Set Content-Length to match the actual body we're sending
        builder = builder.header(http::header::CONTENT_LENGTH, body.len());

        // For converted 200: let CEF cache the full resource normally
        if is_full_resource_206 {
          builder = builder.header(
            http::header::CACHE_CONTROL,
            "public, max-age=31536000, immutable",
          );
        }
        // For genuinely partial 206: force no caching
        else if status == http::StatusCode::PARTIAL_CONTENT {
          builder = builder
            .header(
              http::header::CACHE_CONTROL,
              "no-store, no-cache, must-revalidate",
            )
            .header(http::header::PRAGMA, "no-cache");
        }

        builder.status(final_status).body(body.to_vec().into())?
      }
      Err(e) => {
        let error_message = format!(
          "Failed to request {}: {}{}",
          url.as_str(),
          e,
          if let Some(s) = e.status() {
            format!("status code: {}", s.as_u16())
          } else if cfg!(target_os = "ios") {
            ", did you grant local network permissions? That is required to reach the development server. Please grant the permission via the prompt or in `Settings > Privacy & Security > Local Network` and restart the app. See https://support.apple.com/en-us/102229 for more information.".to_string()
          } else {
            "".to_string()
          }
        );
        log::error!("{error_message}");
        return Err(error_message.into());
      }
    }
  } else {
    let use_https_scheme =
      request.uri().scheme() == Some(&http::uri::Scheme::HTTPS);
    let asset = manager.get_asset(path, use_https_scheme)?;
    builder = builder.header(CONTENT_TYPE, &asset.mime_type);
    if let Some(csp) = &asset.csp_header {
      builder = builder.header("Content-Security-Policy", csp);
    }
    builder.body(asset.bytes.into())?
  };

  if let Some(handler) = &web_resource_request_handler {
    handler(request, &mut response);
  }

  Ok(response)
}
