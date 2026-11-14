use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

pub(crate) type Requests = Arc<Mutex<Vec<(String, Value)>>>;
pub(crate) struct Http {
    pub url: String,
    pub requests: Requests,
    task: JoinHandle<()>,
}
impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Http {
    pub async fn new(reply: impl Fn(&str, &Value) -> (u16, Value) + Send + Sync + 'static) -> Self {
        Self::with_reply_bytes(move |path, body| {
            let (status, body) = reply(path, body);
            (status, serde_json::to_vec(&body).unwrap())
        })
        .await
    }
    pub async fn with_reply_bytes(
        reply: impl Fn(&str, &Value) -> (u16, Vec<u8>) + Send + Sync + 'static,
    ) -> Self {
        Self::with_async_reply(move |path, body| {
            let response = reply(&path, &body);
            std::future::ready(response)
        })
        .await
    }
    pub async fn with_async_reply<F, R>(reply: F) -> Self
    where
        F: Fn(String, Value) -> R + Send + Sync + 'static,
        R: std::future::Future<Output = (u16, Vec<u8>)> + Send,
    {
        Self::with_reply_mode(reply, false).await
    }
    pub async fn with_concurrent_reply<F, R>(reply: F) -> Self
    where
        F: Fn(String, Value) -> R + Send + Sync + 'static,
        R: std::future::Future<Output = (u16, Vec<u8>)> + Send,
    {
        Self::with_reply_mode(reply, true).await
    }
    async fn with_reply_mode<F, R>(reply: F, concurrent: bool) -> Self
    where
        F: Fn(String, Value) -> R + Send + Sync + 'static,
        R: std::future::Future<Output = (u16, Vec<u8>)> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Requests::default();
        let captured = requests.clone();
        let reply = Arc::new(reply);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let captured = captured.clone();
                let reply = reply.clone();
                connections.spawn(async move {
                let mut head = Vec::new();
                loop {
                    let mut byte = [0];
                    if socket.read_exact(&mut byte).await.is_err() {
                        break;
                    }
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(head.len() < 32768);
                }
                let text = String::from_utf8_lossy(&head);
                let Some(path) = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                else {
                    return;
                };
                let len = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|len| len.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; len];
                socket.read_exact(&mut body).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push((path.to_owned(), body.clone()));
                let (status, body) = reply(path.to_owned(), body).await;
                let head = format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                });
                if !concurrent {
                    connections.join_next().await.unwrap().unwrap();
                } else {
                    while let Some(result) = connections.try_join_next() {
                        result.unwrap();
                    }
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
