use std::time::Duration;

use serde::Serialize;
use tauri::{Manager, ResourceId, Runtime, Webview};
use tauri_plugin_updater::{Update, UpdaterBuilder, UpdaterExt};

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateMetadata {
    rid: ResourceId,
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
    raw_json: serde_json::Value,
}

async fn fetch_update(
    builder: UpdaterBuilder,
    check_timeout: Duration,
    connect_timeout: Duration,
) -> Result<Option<Update>, String> {
    let mut update = builder
        .timeout(check_timeout)
        // A dead CDN address must yield to the next resolved address instead
        // of waiting for the operating system's minute-long TCP timeout.
        .configure_client(move |client| client.connect_timeout(connect_timeout))
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(update) = &mut update {
        // Keep the connection limit for downloads, but not the short deadline
        // intended for fetching a small update feed.
        update.timeout = None;
    }
    Ok(update)
}

fn register_update<R: Runtime>(
    webview: &Webview<R>,
    update: Update,
) -> Result<UpdateMetadata, String> {
    let date = update
        .date
        .map(|date| date.format(&time::format_description::well_known::Rfc3339))
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(UpdateMetadata {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date,
        body: update.body.clone(),
        raw_json: update.raw_json.clone(),
        // Keep Tauri's native resource so its download/install commands still
        // verify the configured signing key and perform the platform install.
        rid: webview.resources_table().add(update),
    })
}

#[tauri::command]
pub(crate) async fn check_for_update<R: Runtime>(
    webview: Webview<R>,
) -> Result<Option<UpdateMetadata>, String> {
    fetch_update(webview.updater_builder(), CHECK_TIMEOUT, CONNECT_TIMEOUT)
        .await?
        .map(|update| register_update(&webview, update))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};

    fn app(endpoint: &str) -> tauri::App<MockRuntime> {
        let mut context = mock_context(noop_assets());
        context.config_mut().plugins.0.insert(
            "updater".into(),
            serde_json::json!({
                "pubkey": "test-only-key",
                "endpoints": [endpoint],
                "dangerousInsecureTransportProtocol": true
            }),
        );
        mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap()
    }

    #[test]
    fn stalled_feed_finishes_at_the_check_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = app(&format!("http://{}/feed", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(600));
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        });
        let start = Instant::now();
        let result = tauri::async_runtime::block_on(fetch_update(
            app.updater_builder().no_proxy(),
            Duration::from_millis(100),
            Duration::from_secs(1),
        ));
        let elapsed = start.elapsed();
        server.join().unwrap();
        assert!(result.is_err(), "A stalled feed must not report up to date");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn stalled_tls_connection_has_a_shorter_deadline_than_the_feed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = app(&format!("https://{}/feed", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(600));
        });
        let start = Instant::now();
        let result = tauri::async_runtime::block_on(fetch_update(
            app.updater_builder().no_proxy(),
            Duration::from_secs(2),
            Duration::from_millis(100),
        ));
        let elapsed = start.elapsed();
        server.join().unwrap();
        assert!(result.is_err());
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn redirected_feed_keeps_the_native_install_resource_without_check_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = app(&format!("{endpoint}/latest"));
        let server = thread::spawn(move || {
            for path in ["/latest", "/feed"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..size]).starts_with(&format!("GET {path} "))
                );
                let response = if path == "/latest" {
                    format!("HTTP/1.1 302 Found\r\nLocation: {endpoint}/feed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                } else {
                    let body = serde_json::json!({
                        "version": "99.0.0",
                        "pub_date": "2026-09-15T00:00:00Z",
                        "notes": "Fork release notes",
                        "url": format!("{endpoint}/app.tar.gz"),
                        "signature": "signed-update"
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let update = tauri::async_runtime::block_on(fetch_update(
            app.updater_builder().no_proxy(),
            CHECK_TIMEOUT,
            CONNECT_TIMEOUT,
        ))
        .unwrap()
        .unwrap();
        server.join().unwrap();
        let webview = tauri::WebviewWindowBuilder::new(&app, "test", tauri::WebviewUrl::default())
            .build()
            .unwrap()
            .as_ref()
            .clone();
        let metadata = register_update(&webview, update).unwrap();
        let native = webview
            .resources_table()
            .get::<Update>(metadata.rid)
            .unwrap();
        assert_eq!(native.version, "99.0.0");
        assert_eq!(native.signature, "signed-update");
        assert_eq!(
            native.timeout, None,
            "Large downloads must outlive the feed deadline"
        );
        let json = serde_json::to_value(metadata).unwrap();
        assert_eq!(json["currentVersion"], "0.1.0");
        assert_eq!(json["date"], "2026-09-15T00:00:00Z");
        assert_eq!(json["body"], "Fork release notes");
        assert_eq!(json["rawJson"]["version"], "99.0.0");
    }
}
