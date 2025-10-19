use fast_websocket_client::proxy::ProxyBuilder;
use fast_websocket_client::{ConnectionInitOptions, WebSocketBuilder};
use tokio::time::{Duration, sleep};

async fn roundtrip(opts: Option<ConnectionInitOptions>) {
    let url = "wss://echo.websocket.org";
    let ws = match opts {
        Some(o) => WebSocketBuilder::new().with_options(o).connect(url).await,
        None => WebSocketBuilder::new().connect(url).await,
    }
    .expect("could not connect");

    ws.send("hi").await.unwrap();
    sleep(Duration::from_millis(200)).await;
    ws.close().await;
    ws.await_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct() {
    roundtrip(None).await;
}

#[cfg(feature = "proxy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_proxy() {
    let proxy = ProxyBuilder::new()
        .http("http://localhost:3128")
        .unwrap()
        .build()
        .unwrap();
    roundtrip(Some(ConnectionInitOptions::new().proxy(Some(proxy)))).await;
}

#[cfg(feature = "proxy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks5_proxy() {
    let proxy = ProxyBuilder::new()
        .socks5("localhost:1080")
        .auth("myuser", "mypassword")
        .build()
        .unwrap();
    roundtrip(Some(ConnectionInitOptions::new().proxy(Some(proxy)))).await;
}
