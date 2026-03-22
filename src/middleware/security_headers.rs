use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

pub async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let is_https = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v == "https")
        || request.uri().scheme_str() == Some("https");

    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("X-Frame-Options", "DENY".parse().unwrap());
    headers.insert("Referrer-Policy", "no-referrer".parse().unwrap());
    headers.insert(
        "Permissions-Policy",
        "geolocation=(), microphone=(), camera=(), payment=(), usb=()"
            .parse()
            .unwrap(),
    );

    if is_https {
        headers.insert(
            "Strict-Transport-Security",
            "max-age=31536000; includeSubDomains".parse().unwrap(),
        );
    }

    response
}
