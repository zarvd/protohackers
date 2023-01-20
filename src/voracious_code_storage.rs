use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

struct INode {
    content: Bytes,
    revision: u64,
    children: HashMap<String, INode>, // An Inode could be both directory and file
}

impl INode {
    pub fn dir() -> Self {
        Self {
            content: Bytes::new(),
            revision: 0,
            children: HashMap::new(),
        }
    }

    pub fn write_file(&mut self, path: String, content: Bytes) -> Result<u64> {
        let (dirs, filename) = Self::split_path_with_filename(path);

        let dir = self.mkdir(dirs)?;
        let f = dir.children.entry(filename).or_insert_with(|| INode {
            content: content.clone(),
            revision: 1,
            children: HashMap::new(),
        });
        if content != f.content {
            f.revision += 1;
        }
        Ok(f.revision)
    }

    #[inline]
    pub fn get_file(&self, path: String) -> Option<Bytes> {
        let (dirs, filename) = Self::split_path_with_filename(path);
        self.cd_dir(dirs)
            .ok()?
            .children
            .get(&filename)
            .map(|f| f.content.clone())
    }

    #[inline]
    pub fn list_dir(&self, path: String) -> Vec<String> {
        let dirs = Self::split_path(path);
        match self.cd_dir(dirs).ok() {
            Some(d) => d
                .children
                .iter()
                .map(|(filename, inode)| {
                    format!(
                        "{} {}",
                        filename,
                        if inode.children.is_empty() {
                            format!("r{}", inode.revision)
                        } else {
                            "DIR".to_string()
                        }
                    )
                })
                .collect(),
            None => vec![],
        }
    }

    #[inline]
    fn mkdir(&mut self, dirs: Vec<String>) -> Result<&mut Self> {
        let mut dir = self;
        for d in dirs {
            dir = dir.children.entry(d).or_insert_with(Self::dir);
        }

        Ok(dir)
    }

    #[inline]
    fn cd_dir(&self, dirs: Vec<String>) -> Result<&Self> {
        let mut dir = self;
        for d in dirs {
            match dir.children.get(&d) {
                Some(next) => dir = next,
                None => bail!("{} dir doesn't exist", d),
            }
        }

        Ok(dir)
    }

    #[inline]
    fn split_path(path: String) -> Vec<String> {
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        parts.into_iter().map(|s| s.to_string()).collect()
    }

    #[inline]
    fn split_path_with_filename(path: String) -> (Vec<String>, String) {
        let mut dirs = Self::split_path(path);
        let filename = dirs.pop().unwrap();
        (dirs, filename)
    }
}

#[derive(Clone)]
struct State {
    inode: Arc<RwLock<INode>>,
}

impl State {
    pub fn new() -> Self {
        Self {
            inode: Arc::new(RwLock::new(INode::dir())),
        }
    }

    pub fn put(&mut self, path: String, content: Bytes) -> u64 {
        let mut root = self.inode.write().unwrap();
        root.write_file(path, content).expect("write to file")
    }

    pub fn get(&mut self, path: String) -> Option<Bytes> {
        let root = self.inode.read().unwrap();
        root.get_file(path)
    }

    pub fn list(&mut self, path: String) -> Vec<String> {
        let root = self.inode.read().unwrap();
        root.list_dir(path)
    }
}

enum Request {
    PutFile { path: String, content: Bytes },
    GetFile { path: String },
    ListDir { path: String },
    Closed,
}

enum Error {
    FileNotFound,
}

impl Into<Bytes> for Error {
    fn into(self) -> Bytes {
        match self {
            Self::FileNotFound => Bytes::from_static(b"no such file"),
        }
    }
}

enum Response {
    Ready,
    FileContent(Bytes),
    FileRevision(u64),
    Files(Vec<String>),
    Err(Error),
}

impl Into<Bytes> for Response {
    fn into(self) -> Bytes {
        match self {
            Self::Ready => Bytes::from_static(b"READY\n"),
            Self::FileContent(content) => {
                let mut buf = BytesMut::from(format!("OK {}\n", content.len()).as_bytes());
                buf.put(content);
                buf.put(&b"READY\n"[..]);
                buf.freeze()
            }
            Self::FileRevision(revision) => {
                let mut buf = BytesMut::from(format!("OK r{}\n", revision).as_bytes());
                buf.put(&b"READY\n"[..]);
                buf.freeze()
            }
            Self::Files(files) => {
                let mut buf = BytesMut::from(format!("OK {}\n", files.len()).as_bytes());
                for f in files {
                    buf.put_slice(f.as_bytes());
                }
                buf.freeze()
            }
            Self::Err(e) => {
                let mut buf = BytesMut::from(&b"ERR "[..]);
                buf.put::<Bytes>(e.into());
                buf.put_u8(b'\n');
                buf.put(&b"READY\n"[..]);
                buf.freeze()
            }
        }
    }
}

struct Context<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> Context<R, W>
where
    R: Unpin + AsyncRead + AsyncBufReadExt,
    W: Unpin + AsyncWrite,
{
    pub fn new(r: R, w: W) -> Self {
        Self {
            reader: r,
            writer: w,
        }
    }

    pub async fn incoming(&mut self) -> Result<Request> {
        let mut buf = String::new();
        let req = match self.reader.read_line(&mut buf).await? {
            0 => Request::Closed,
            _ => {
                buf.pop();
                info!("Received incoming message: '{}'", buf);
                let parts: Vec<_> = buf.split(' ').collect();
                match parts[0] {
                    "PUT" => {
                        let content_len = parts[2].parse::<usize>().unwrap();
                        let mut content = vec![0; content_len];
                        self.reader.read_exact(&mut content).await?;
                        Request::PutFile {
                            path: parts[1].to_string(),
                            content: Bytes::from(content),
                        }
                    }
                    "GET" => Request::GetFile {
                        path: parts[1].to_string(),
                    },
                    "LIST" => Request::ListDir {
                        path: parts[1].to_string(),
                    },
                    typ => {
                        bail!("unknown request type: {}", typ);
                    }
                }
            }
        };
        Ok(req)
    }

    pub async fn outgoing(&mut self, response: Response) -> Result<()> {
        let data: Bytes = response.into();
        info!("<- Server: {}", unsafe {
            String::from_utf8_unchecked(data.clone().to_vec()).replace('\n', "<NL>")
        });
        self.writer.write_all(&data).await?;
        Ok(())
    }
}

struct Upstream {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Upstream {
    pub async fn connect() -> Result<Self> {
        const ADDR: &'static str = "vcs.protohackers.com:30307";
        let (rh, wh) = TcpStream::connect(ADDR).await?.into_split();
        Ok(Self {
            reader: BufReader::new(rh),
            writer: wh,
        })
    }

    pub async fn send(&mut self, req: Request) -> Result<Response> {
        let resp = match req {
            Request::PutFile {
                path: filename,
                content,
            } => {
                let mut data = format!("PUT {} {}\n", filename, content.len()).into_bytes();
                data.extend(content);
                info!("Sending {} to Upstream", unsafe {
                    String::from_utf8_unchecked(data.clone())
                });
                self.writer.write_all(&data).await?;

                let mut resp = String::new();
                self.reader.read_line(&mut resp).await?;
                info!("Received {} from Upstream", resp);
                Response::Ready
            }
            _ => Response::Ready,
        };
        Ok(resp)
    }
}

async fn handle(mut socket: TcpStream, remote_addr: SocketAddr, mut state: State) -> Result<()> {
    let mut ctx = {
        let (rh, wh) = socket.split();
        let rh = BufReader::new(rh);
        Context::new(rh, wh)
    };

    // send greeting message
    ctx.outgoing(Response::Ready).await?;

    loop {
        match ctx.incoming().await? {
            Request::PutFile { path, content } => {
                info!(
                    "{} -> Server: PUT {} with content '{}'",
                    remote_addr,
                    path,
                    unsafe {
                        String::from_utf8_unchecked(content.clone().to_vec()).replace('\n', "<NL>")
                    }
                );
                let revision = state.put(path, content);
                info!("{} <- Server: OK with revision {}", remote_addr, revision);
                ctx.outgoing(Response::FileRevision(revision)).await?;
            }
            Request::GetFile { path } => {
                info!("{} -> Server: GET {}", remote_addr, path);
                let resp = match state.get(path) {
                    Some(content) => {
                        info!("{} <- Server: OK with content '{}'", remote_addr, unsafe {
                            String::from_utf8_unchecked(content.clone().to_vec())
                                .replace('\n', "<NL>")
                        });
                        Response::FileContent(content)
                    }
                    None => {
                        info!("{} <- Server: Error because file not found", remote_addr);
                        Response::Err(Error::FileNotFound)
                    }
                };

                ctx.outgoing(resp).await?;
            }
            Request::ListDir { path } => {
                info!("{} -> Server: LIST {}", remote_addr, path);
                ctx.outgoing(Response::Files(state.list(path))).await?;
            }
            Request::Closed => {
                info!("Closing the connection");
                break;
            }
        }
    }

    Ok(())
}

pub async fn run(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("TCP Server listening on {}", addr);

    let state = State::new();
    loop {
        let state = state.clone();
        match listener.accept().await {
            Ok((socket, remote_addr)) => {
                tokio::spawn(async move {
                    info!("Accepting socket from {}", remote_addr);
                    let _ = handle(socket, remote_addr, state).await;
                    info!("Dropping socket {}", remote_addr);
                });
            }
            Err(e) => {
                error!("Failed to accept socket: {:?}", e);
            }
        }
    }

    // {
    //     let mut upstream = Upstream::connect().await?;
    //
    //     {
    //         let mut buf = String::new();
    //         upstream.reader.read_line(&mut buf).await?;
    //         info!("Greet from upstream: '{}'", buf);
    //     }
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/foo.txt".to_string(),
    //             content: Bytes::from_static(b"Hello, world"),
    //         })
    //         .await?;
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/bar.txt".to_string(),
    //             content: Bytes::from_static(b"damn you"),
    //         })
    //         .await?;
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/ccc/bar.txt".to_string(),
    //             content: Bytes::from_static(b"damn you"),
    //         })
    //         .await?;
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/z/bar.txt".to_string(),
    //             content: Bytes::from_static(b"damn you"),
    //         })
    //         .await?;
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/z".to_string(),
    //             content: Bytes::from_static(b"damn you"),
    //         })
    //         .await?;
    //
    //     upstream
    //         .send(Request::PutFile {
    //             path: "/x/bar.txt".to_string(),
    //             content: Bytes::from_static(b"damn you"),
    //         })
    //         .await?;
    //
    //     info!("Listing directory");
    //
    //     upstream.writer.write_all(b"LIST /\n").await?;
    //     upstream.writer.write_all(b"LIST /x/ba\n").await?;
    //     upstream.writer.write_all(b"GET /z/bar.txt\n").await?;
    //
    //     loop {
    //         let mut buf = String::new();
    //         upstream.reader.read_line(&mut buf).await?;
    //         info!("Received from upstream: '{}'", buf);
    //     }
    //     Ok(())
    // }
}
