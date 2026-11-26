//! Client side of the daemon socket, used by the TUI and the CLI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::config::Paths;
use crate::proto::{Command, Envelope, Request, ServerMsg};
use crate::service;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct Conn {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    events: Option<mpsc::UnboundedReceiver<Envelope>>,
}

impl Conn {
    pub async fn connect(paths: &Paths) -> Result<Conn> {
        let stream = UnixStream::connect(paths.socket())
            .await
            .with_context(|| format!("connecting to {}", paths.socket().display()))?;
        let (read, mut write) = stream.into_split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Envelope>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });
        let waiting = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMsg>(&line) {
                    Ok(ServerMsg::Reply { id, ok, data, error }) => {
                        let mut waiting = waiting.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(tx) = waiting.remove(&id) {
                            let _ = tx.send(if ok { Ok(data) } else { Err(error) });
                        } else if !ok {
                            // The daemon could not read the request, so it cannot say
                            // which one failed. Fail them all rather than hang.
                            for (_, tx) in waiting.drain() {
                                let _ = tx.send(Err(error.clone()));
                            }
                        }
                    }
                    Ok(ServerMsg::Push(envelope)) => {
                        let _ = ev_tx.send(envelope);
                    }
                    Err(_) => {}
                }
            }
            // Connection lost: fail whatever is still waiting, and close the event stream.
            for (_, tx) in waiting.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                let _ = tx.send(Err("the daemon closed the connection".into()));
            }
        });
        Ok(Conn { out, pending, next_id: AtomicU64::new(1), events: Some(ev_rx) })
    }

    /// Connect, starting the daemon first if it is not running.
    pub async fn connect_or_start(paths: &Paths) -> Result<Conn> {
        if let Ok(conn) = Conn::connect(paths).await {
            if conn.call(Command::Ping).await.is_ok() {
                return Ok(conn);
            }
        }
        service::start(paths, true).await?;
        Conn::connect(paths).await
    }

    pub async fn call(&self, cmd: Command) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let line = serde_json::to_string(&Request { id, cmd })?;
        if self.out.send(line).is_err() {
            bail!("not connected to the daemon");
        }
        match tokio::time::timeout(Duration::from_secs(600), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => bail!("the daemon closed the connection"),
            Err(_) => bail!("the daemon did not answer in time"),
        }
    }

    /// The stream of pushed events. Can be taken once.
    pub fn take_events(&mut self) -> mpsc::UnboundedReceiver<Envelope> {
        self.events.take().expect("events already taken")
    }
}
