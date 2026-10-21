use std::future::Future;
use std::time::Duration;

use tokio::sync::oneshot;

/// `server` (graceful shutdown 付きの serve) を完了まで待つ。ただし drain
/// (処理中リクエストの完了待ち) は `signal_received` を受けてから `drain_timeout` で打ち切る。
/// drain が間に合えば `server` の結果を、打ち切った場合は `None` を返す。
///
/// 打ち切ると `server` は drop されるが、axum が接続ごとに spawn したタスクと
/// dump の refresh タスクは、呼び出し元の flush 中も動き続け、main を抜けて runtime が
/// drop される時点で drop される (mariadb-dump は kill_on_drop で kill される)。
/// キャッシュはメモリ上にしかなく、dump が最後まで成功したときにだけ置き換えるので、
/// 中断しても不整合は残らない。打ち切ったリクエストのスパンは provider の shutdown 後に
/// 終わるため送信されない
pub async fn serve_with_drain_timeout<F: Future>(
    server: F,
    signal_received: oneshot::Receiver<()>,
    drain_timeout: Duration,
) -> Option<F::Output> {
    let drain_deadline = async {
        // sender は server 内の shutdown signal future が持つ。signal を送らずに drop
        // されるのは server が終わるときなので、Err でも同じく drain_timeout だけ待てばよい
        let _ = signal_received.await;
        tokio::time::sleep(drain_timeout).await;
    };
    tokio::select! {
        result = server => Some(result),
        () = drain_deadline => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRAIN: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn returns_server_result_without_signal() {
        let (_tx, rx) = oneshot::channel();
        assert_eq!(
            serve_with_drain_timeout(async { 42 }, rx, DRAIN).await,
            Some(42)
        );
    }

    #[tokio::test]
    async fn waits_for_drain_that_finishes_in_time() {
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();
        let server = async {
            tokio::time::sleep(DRAIN / 2).await;
            42
        };
        assert_eq!(serve_with_drain_timeout(server, rx, DRAIN).await, Some(42));
    }

    #[tokio::test]
    async fn gives_up_drain_after_timeout() {
        let (tx, rx) = oneshot::channel();
        // signal を受けてから数えるので、受信前の時間は drain に含まれない
        let server = async move {
            tokio::time::sleep(DRAIN * 2).await;
            tx.send(()).unwrap();
            std::future::pending::<()>().await
        };
        let started = std::time::Instant::now();
        assert_eq!(serve_with_drain_timeout(server, rx, DRAIN).await, None);
        assert!(started.elapsed() >= DRAIN * 3);
    }
}
