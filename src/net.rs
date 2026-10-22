//! M4a WiFi 导入:局域网内手机浏览器上传照片到本机 Incoming 目录。
//! 手写极简 HTTP(仅支持 POST /upload,头 X-Filename + Content-Length),
//! 收到文件后增量扫描 Incoming 并重建缩略图。无第三方 HTTP 依赖。
use crate::db::Store;
use anyhow::Result;
use std::io::Write;
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const PORT: u16 = 8422;
const MAX_BODY: u64 = 512 * 1024 * 1024;

pub struct WifiHandle {
    pub stop: Arc<AtomicBool>,
    pub received: Arc<AtomicUsize>,
    pub addr: String,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WifiHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WifiHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 不 join:停止标志轮询期 ≤ 1s,进程退出自然回收
    }
}

/// 局域网 IPv4 地址(UDP connect 技巧,不发包)。
pub fn lan_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

pub fn incoming_dir() -> PathBuf {
    crate::db::data_dir().join("Incoming")
}

/// 启动监听线程。viewer_status 回传 fn wifi_status(store)。
pub fn start(store: Arc<Store>) -> Result<WifiHandle> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    let stop2 = stop.clone();
    let recv2 = received.clone();
    let addr = format!("{}:{PORT}", lan_ip().unwrap_or_else(|| "?".into()));
    let thread = std::thread::spawn(move || {
        let incoming = incoming_dir();
        let _ = std::fs::create_dir_all(&incoming);
        while !stop2.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut sock, _peer)) => {
                    sock.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
                    match handle_one(&mut sock, &incoming) {
                        Some(crate::net::Reply::UploadOk) => {
                            let n = recv2.fetch_add(1, Ordering::SeqCst) + 1;
                            // 增量扫描 Incoming(单根、小目录,串行即可)
                            if let Err(e) = crate::scan::scan_dir(&store, &[incoming.clone()]) {
                                eprintln!("incoming 扫描失败: {e}");
                            }
                            let _ = crate::thumbs::backfill(&store, 8);
                            let body = format!("已接收 {n} 张");
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = sock.write_all(resp.as_bytes());
                            let _ = sock.flush();
                        }
                        Some(crate::net::Reply::Html) => {}
                        None => {
                            let _ = sock.write_all(
                                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                            let _ = sock.flush();
                        }
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
    });
    Ok(WifiHandle {
        stop,
        received,
        addr,
        thread: Some(thread),
    })
}

enum Reply {
    /// GET / :HTML 页已在本函数内回复
    Html,
    /// POST /upload 成功
    UploadOk,
}

fn handle_one(sock: &mut std::net::TcpStream, incoming: &std::path::Path) -> Option<Reply> {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(sock);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    if request_line.starts_with("GET ") {
        // 丢弃剩余头
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                _ => {}
            }
            if line.trim_end().is_empty() {
                break;
            }
        }
        let body = INDEX_HTML;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = reader.get_mut().write_all(resp.as_bytes());
        let _ = reader.get_mut().flush();
        return Some(Reply::Html);
    }
    if !request_line.starts_with("POST /upload") {
        return None;
    }
    let mut name: Option<String> = None;
    let mut clen: Option<u64> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("x-filename:") {
            name = line[11..].trim().to_string().into();
        } else if lower.starts_with("content-length:") {
            clen = line[15..].trim().parse().ok();
        }
    }
    let clen = clen?;
    if !(1..=MAX_BODY).contains(&clen) {
        return None;
    }
    let raw = name?;
    let (safe, ext_ok) = sanitize_name(&raw);
    if !ext_ok {
        return None;
    }
    let mut body = vec![0u8; clen as usize];
    reader.read_exact(&mut body).ok()?;
    drop(reader);
    let target = free_target(incoming, &safe);
    if std::fs::write(&target, &body).is_err() {
        return None;
    }
    Some(Reply::UploadOk)
}

const INDEX_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Maliang 照片导入</title>
<meta name="viewport" content="width=device-width,initial-scale=1"></head>
<body><h3>Maliang WiFi 照片导入</h3>
<input type="file" multiple accept="image/*" id="f" onchange="send()"><p id="s"></p>
<script>
function send(){
 var fs=document.getElementById('f').files;var s=document.getElementById('s');var i=0;
 (function next(){
  if(i>=fs.length){s.textContent='完成 '+fs.length+' 张';return;}
  var f=fs[i];
  fetch('/upload',{method:'POST',headers:{'X-Filename':encodeURIComponent(f.name)},body:f})
   .then(function(){i++;s.textContent=(i)+'/'+fs.length+' …';next();})
   .catch(function(e){s.textContent='失败:'+e;});
 })();
}
</script></body></html>"#;

fn sanitize_name(raw: &str) -> (PathBuf, bool) {
    // URL 解码(按字节,避免多字节字符错位)→ 只留文件名;防穿越;仅接受常见图像扩展
    let bytes = raw.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                decoded.push(b);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&decoded).into_owned();
    let name = decoded
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let name = name.replace(':', "_").replace("..", "_");
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ok = matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tiff" | "gif"
    );
    (PathBuf::from(name), ok)
}

fn free_target(dest: &std::path::Path, name: &std::path::Path) -> PathBuf {
    let stem = name
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = name
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 0.. {
        let cand = if n == 0 {
            dest.join(name)
        } else {
            dest.join(format!("{stem} ({n}){ext}"))
        };
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal() {
        let (p, ok) = sanitize_name("a%2F..%2Fb%2Fphoto%20x.jpg");
        assert!(ok);
        assert_eq!(p, PathBuf::from("photo x.jpg"));
        let (_, bad) = sanitize_name("evil.txt");
        assert!(!bad);
    }

    #[test]
    fn upload_roundtrip() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("maliang-net-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sdir = dir.clone();
        let t = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            handle_one(&mut sock, &sdir)
        });
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = b"HELLO";
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: x\r\nX-Filename: hello%20a.jpg\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        c.write_all(req.as_bytes()).unwrap();
        c.write_all(body).unwrap();
        let reply = t.join().unwrap();
        assert!(matches!(reply, Some(Reply::UploadOk)));
        let got = std::fs::read(dir.join("hello a.jpg")).unwrap();
        assert_eq!(got, b"HELLO");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn get_serves_html() {
        use std::io::Read as _;
        let dir = std::env::temp_dir().join(format!("maliang-net-get-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sdir = dir.clone();
        let t = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            handle_one(&mut sock, &sdir)
        });
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let reply = t.join().unwrap();
        assert!(matches!(reply, Some(Reply::Html)));
        let mut buf = String::new();
        let _ = c.read_to_string(&mut buf);
        assert!(buf.contains("</html>"), "resp={buf}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
