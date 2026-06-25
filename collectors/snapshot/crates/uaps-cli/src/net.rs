//! FS-independent per-rank rendezvous over TCP.
//!
//! The parent (per-rank launcher) listens; each rank connects and sends its
//! snapshot. TCP guarantees integrity and ordering, so this works with **no shared
//! filesystem** — unlike a shared-FS rendezvous (needs a cluster FS) or funnelling
//! through the launcher's stderr (which splits/interleaves structured output and is
//! therefore unreliable). If TCP is unreachable (firewall) a rank simply doesn't
//! report and the parent's world-size check flags it — never a silent wrong number.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Rank side: send `rank` + `json` to the parent at `addr` (`host:port`).
/// Best-effort with quick retries (the parent binds before launching, so it is
/// normally already listening). Frame: `[rank u32 LE][len u32 LE][json bytes]`.
pub fn send_snapshot(addr: &str, rank: i64, json: &str) -> std::io::Result<()> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..40 {
        match TcpStream::connect(addr) {
            Ok(mut s) => {
                s.set_write_timeout(Some(Duration::from_secs(15))).ok();
                let body = json.as_bytes();
                s.write_all(&(rank as u32).to_le_bytes())?;
                s.write_all(&(body.len() as u32).to_le_bytes())?;
                s.write_all(body)?;
                s.flush()?;
                return Ok(());
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "connect failed")))
}

/// Parent side: bind a collector listener, returning it and the `host:port` string
/// ranks should dial. Binds all interfaces; advertises this node's hostname (which
/// is reachable from the other ranks — the launcher already ssh'd between them).
pub fn bind_collector() -> std::io::Result<(TcpListener, String)> {
    use std::net::ToSocketAddrs;
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let port = listener.local_addr()?.port();
    let host = std::env::var("UAPS_COLLECT_HOST").ok().unwrap_or_else(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_string())
            .unwrap_or_default()
    });
    // The listener is on all interfaces, so any address that resolves to this host
    // reaches it. Use the hostname when it resolves (needed for remote ranks on a
    // cluster); fall back to loopback when it doesn't (e.g. a CI runner whose
    // hostname isn't in DNS/hosts — single-node, so loopback is enough).
    let resolves = !host.is_empty()
        && (host.as_str(), port)
            .to_socket_addrs()
            .map(|mut a| a.next().is_some())
            .unwrap_or(false);
    let addr = if resolves {
        format!("{host}:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };
    Ok((listener, addr))
}

/// Parent side: accept rank connections until `stop` is set (plus a short drain for
/// stragglers), writing each rank's snapshot to `dir`/snap.<rank>.json. Returns the
/// number of ranks received.
pub fn collect_into(listener: TcpListener, dir: &Path, stop: Arc<AtomicBool>) -> usize {
    listener.set_nonblocking(true).ok();
    let mut received = 0usize;
    let mut drain_until: Option<Instant> = None;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if read_one(stream, dir) {
                    received += 1;
                }
                drain_until = None; // saw activity; reset the post-stop grace window
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) {
                    let until = *drain_until.get_or_insert_with(|| Instant::now() + Duration::from_millis(750));
                    if Instant::now() >= until {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    received
}

fn read_one(mut s: TcpStream, dir: &Path) -> bool {
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut hdr = [0u8; 8];
    if s.read_exact(&mut hdr).is_err() {
        return false;
    }
    let rank = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as i64;
    let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    if len == 0 || len > 64 * 1024 * 1024 {
        return false;
    }
    let mut body = vec![0u8; len];
    if s.read_exact(&mut body).is_err() {
        return false;
    }
    std::fs::write(dir.join(format!("snap.{rank}.json")), &body).is_ok()
}
