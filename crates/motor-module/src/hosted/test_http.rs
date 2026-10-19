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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Requests::default();
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
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
                    continue;
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
                let (status, body) = reply(path, &body);
                let body = serde_json::to_vec(&body).unwrap();
                let head = format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
