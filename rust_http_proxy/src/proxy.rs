use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::{Display, Formatter},
    io::{self, ErrorKind},
    net::SocketAddr,
    time::Duration,
};

use crate::{ip_x::SocketAddrFormat, net_monitor::NetMonitor, web_func, Config, LOCAL_IP};
use {io_x::CounterIO, io_x::TimeoutIO, prom_label::LabelImpl};

use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{
    body::Bytes,
    header::{self, HeaderValue},
    http,
    upgrade::Upgraded,
    Method, Request, Response, Version,
};
use hyper::{
    body::{Body, Incoming},
    client::conn::http1::Builder,
    header::HeaderName,
};
use hyper_util::rt::TokioIo;
use log::{debug, info, warn};
use percent_encoding::percent_decode_str;
use prom_label::Label;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family},
    registry::Registry,
};
use rand::Rng;
use tokio::{net::TcpStream, pin};

pub struct ProxyHandler {
    prom_registry: Registry,
    metrics: Metrics,
    net_monitor: NetMonitor,
}

pub(crate) struct Metrics {
    pub(crate) http_req_counter: Family<LabelImpl<ReqLabels>, Counter>,
    pub(crate) proxy_traffic: Family<LabelImpl<AccessLabel>, Counter>,
    pub(crate) net_bytes: Family<LabelImpl<NetDirectionLabel>, Counter>,
}

impl ProxyHandler {
    pub fn new() -> ProxyHandler {
        let mut registry = Registry::default();
        let metrics = register_metrics(&mut registry);
        let monitor: NetMonitor = NetMonitor::new();
        monitor.start();
        ProxyHandler {
            prom_registry: registry,
            metrics,
            net_monitor: monitor,
        }
    }
    pub async fn proxy(
        &self,
        mut req: Request<hyper::body::Incoming>,
        proxy_config: &'static Config,
        client_socket_addr: SocketAddr,
    ) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
        let config_basic_auth = &proxy_config.basic_auth;
        let never_ask_for_auth = proxy_config.never_ask_for_auth;
        if Method::CONNECT != req.method() {
            let authority = if req.version() == Version::HTTP_2 {
                authority(req.uri()).unwrap_or("".to_owned())
            } else {
                req.headers()
                    .get(http::header::HOST)
                    .map_or("", |h| h.to_str().unwrap_or(""))
                    .to_string()
            };
            if let Some((plaintext_host, plaintext_port)) =
                proxy_config.wrap_plaintexts.get(&authority)
            {
                return self
                    .reverse_proxy(
                        client_socket_addr,
                        authority,
                        req,
                        plaintext_host.as_str(),
                        plaintext_port.to_owned(),
                    )
                    .await;
            } else {
                if req.version() == Version::HTTP_2 || req.uri().host().is_none() {
                    let raw_path = req.uri().path();
                    let path = percent_decode_str(raw_path)
                        .decode_utf8()
                        .unwrap_or(Cow::from(raw_path));
                    let path = path.as_ref();
                    if !config_basic_auth.is_empty() && !never_ask_for_auth {
                        // 存在嗅探风险时，不伪装成http服务
                        return Err(io::Error::new(
                            ErrorKind::PermissionDenied,
                            "reject http GET/POST when ask_for_auth and basic_auth not empty",
                        ));
                    }
                    return web_func::serve_http_request(
                        &req,
                        client_socket_addr,
                        proxy_config,
                        path,
                        &self.net_monitor,
                        &self.metrics,
                        &self.prom_registry,
                    )
                    .await
                    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e));
                }
                if let Some(host) = req.uri().host() {
                    let host = host.to_string();
                    info!(
                        "{:>21?} {:^7} {:?} {:?} Host: {:?} User-Agent: {:?}",
                        client_socket_addr,
                        req.method().as_str(),
                        req.uri(),
                        req.version(),
                        req.headers()
                            .get(http::header::HOST)
                            .map_or("", |h| h.to_str().unwrap_or(host.as_str())),
                        req.headers()
                            .get(http::header::USER_AGENT)
                            .map_or("", |h| h.to_str().unwrap_or("")),
                    );
                };
            }
        }

        let (username, authed) = check_auth(
            config_basic_auth,
            &req,
            &client_socket_addr,
            http::header::PROXY_AUTHORIZATION,
        );
        info!(
            "{:>29} {:<5} {:^8} {:^7} {:?} {:?} ",
            "https://ip.im/".to_owned() + &client_socket_addr.ip().to_canonical().to_string(),
            client_socket_addr.port(),
            username,
            req.method().as_str(),
            req.uri(),
            req.version(),
        );
        if !authed {
            return if never_ask_for_auth {
                Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "wrong basic auth, closing socket...",
                ))
            } else {
                Ok(build_authenticate_resp(true))
            };
        }
        if Method::CONNECT == req.method() {
            // Received an HTTP request like:
            // ```
            // CONNECT www.domain.com:443 HTTP/1.1
            // Host: www.domain.com:443
            // Proxy-Connection: Keep-Alive
            // ```
            //
            // When HTTP method is CONNECT we should return an empty body
            // then we can eventually upgrade the connection and talk a new protocol.
            //
            // Note: only after client received an empty body with STATUS_OK can the
            // connection be upgraded, so we can't return a response inside
            // `on_upgrade` future.
            if let Some(addr) = authority(req.uri()) {
                let proxy_traffic = self.metrics.proxy_traffic.clone();
                tokio::task::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            let access_label = AccessLabel {
                                client: client_socket_addr.ip().to_canonical().to_string(),
                                target: addr.clone(),
                                username,
                            };
                            // Connect to remote server
                            match TcpStream::connect(addr.as_str()).await {
                                Ok(target_stream) => {
                                    let access_tag = access_label.to_string();
                                    let target_stream = CounterIO::new(
                                        target_stream,
                                        proxy_traffic,
                                        LabelImpl::new(access_label),
                                    );
                                    if let Err(e) = tunnel(upgraded, target_stream).await {
                                        // if e.kind() != ErrorKind::TimedOut {
                                        warn!(
                                            "[tunnel io error] [{}]: [{}] {} ",
                                            access_tag,
                                            e.kind(),
                                            e
                                        );
                                        // }
                                    };
                                }
                                Err(e) => {
                                    warn!(
                                        "[tunnel establish error] [{}]: [{}] {} ",
                                        access_label,
                                        e.kind(),
                                        e
                                    )
                                }
                            }
                        }
                        Err(e) => warn!("upgrade error: {}", e),
                    }
                });
                let mut response = Response::new(empty_body());
                // 针对connect请求中，在响应中增加随机长度的padding，防止每次建连时tcp数据长度特征过于敏感
                let max_num = 2048 / LOCAL_IP.len();
                let count = rand::thread_rng().gen_range(1..max_num);
                for _ in 0..count {
                    response
                        .headers_mut()
                        .append(http::header::SERVER, HeaderValue::from_static(&LOCAL_IP));
                }
                Ok(response)
            } else {
                warn!("CONNECT host is not socket addr: {:?}", req.uri());
                let mut resp = Response::new(full_body("CONNECT must be to a socket address"));
                *resp.status_mut() = http::StatusCode::BAD_REQUEST;

                Ok(resp)
            }
        } else {
            // 删除代理特有的请求头
            req.headers_mut()
                .remove(http::header::PROXY_AUTHORIZATION.to_string());
            req.headers_mut().remove("Proxy-Connection");
            let host = req.uri().host().expect("uri has no host");
            let port = req.uri().port_u16().unwrap_or(80);
            let stream = TcpStream::connect((host, port)).await?;
            let server_mod: CounterIO<TcpStream, LabelImpl<AccessLabel>> = CounterIO::new(
                stream,
                self.metrics.proxy_traffic.clone(),
                LabelImpl::new(AccessLabel {
                    client: client_socket_addr.ip().to_canonical().to_string(),
                    target: format!("{}:{}", host, port),
                    username,
                }),
            );
            let io = TokioIo::new(server_mod);
            match Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .handshake(io)
                .await
            {
                Ok((mut sender, conn)) => {
                    tokio::task::spawn(async move {
                        if let Err(err) = conn.await {
                            println!("Connection failed: {:?}", err);
                        }
                    });

                    if let Ok(resp) = sender.send_request(req).await {
                        Ok(resp.map(|b| {
                            b.map_err(|e| {
                                let e = e;
                                io::Error::new(ErrorKind::InvalidData, e)
                            })
                            .boxed()
                        }))
                    } else {
                        Err(io::Error::new(ErrorKind::ConnectionAborted, "连接失败"))
                    }
                }
                Err(e) => Err(io::Error::new(ErrorKind::ConnectionAborted, e)),
            }
        }
    }

    async fn reverse_proxy(
        &self,
        client_socket_addr: SocketAddr,
        authority: String,
        req: Request<Incoming>,
        plain_host: &str,
        plain_port: u16,
    ) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
        let target = format!("{}:{}", plain_host, plain_port);
        info!(
            "{} fetch plaintext of {}:{} through {}",
            SocketAddrFormat(&client_socket_addr),
            plain_host,
            plain_port,
            authority
        );
        let stream = TcpStream::connect((plain_host, plain_port)).await?;
        let stream: CounterIO<TcpStream, LabelImpl<AccessLabel>> = CounterIO::new(
            stream,
            self.metrics.proxy_traffic.clone(),
            LabelImpl::new(AccessLabel {
                client: client_socket_addr.ip().to_canonical().to_string(),
                target: target.clone(),
                username: authority,
            }),
        );
        let stream = TimeoutIO::new(stream, Duration::from_secs(60));
        let io = TokioIo::new(stream);
        match Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(Box::pin(io))
            .await
        {
            Ok((mut sender, conn)) => {
                tokio::task::spawn(async move {
                    if let Err(err) = conn.await {
                        warn!("reverse proxy connection failed: {:?}", err);
                    }
                });

                let method = req.method().clone();
                let url = req.uri().clone();
                let url = match url.path_and_query() {
                    Some(path_and_query) => path_and_query.as_str(),
                    None => "/",
                };
                let mut new_req_builder = Request::builder()
                    .method(method)
                    .uri(url)
                    .version(Version::HTTP_11);
                for ele in req.headers() {
                    new_req_builder = new_req_builder.header(ele.0, ele.1);
                    debug!("{}: {:?}", ele.0, ele.1);
                }

                // let collected = req
                //     .into_body()
                //     .collect()
                //     .await
                //     .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                // // 将收集到的数据转换为字节数组
                // let bytes = collected.to_bytes();
                // info!(
                //     "body is {}",
                //     String::from_utf8(bytes.to_vec()).unwrap_or("".to_string())
                // );
                let mut new_req: Request<Incoming> = new_req_builder
                    .body(req.into_body())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                new_req
                    .headers_mut()
                    .remove(http::header::HOST.to_string());
                new_req.headers_mut().insert(
                    http::header::HOST,
                    HeaderValue::from_str(&target).unwrap_or(HeaderValue::from_static("unknown")),
                );
                if new_req.headers().get(header::CONTENT_LENGTH).is_none()
                    && new_req
                        .headers()
                        .get(header::TRANSFER_ENCODING)
                        .is_none()
                {
                    info!("add header Transfer-Encoding: chunked because of missing of Content-Length for http1.1 protocal");
                    new_req.headers_mut().insert(
                        header::TRANSFER_ENCODING,
                        HeaderValue::from_static("chunked"),
                    );
                }
                // info!("{:?}", new_request);

                if let Ok(resp) = sender.send_request(new_req).await {
                    Ok(resp.map(|b| {
                        b.map_err(|e| {
                            let e = e;
                            io::Error::new(ErrorKind::InvalidData, e)
                        })
                        .boxed()
                    }))
                } else {
                    Err(io::Error::new(ErrorKind::ConnectionAborted, "连接失败"))
                }
            }
            Err(e) => Err(io::Error::new(ErrorKind::ConnectionAborted, e)),
        }
    }
}

fn register_metrics(registry: &mut Registry) -> Metrics {
    let http_req_counter = Family::<LabelImpl<ReqLabels>, Counter>::default();
    registry.register(
        "req_from_out",
        "Number of HTTP requests received",
        http_req_counter.clone(),
    );
    let proxy_traffic = Family::<LabelImpl<AccessLabel>, Counter>::default();
    registry.register("proxy_traffic", "num proxy_traffic", proxy_traffic.clone());
    let net_bytes = Family::<LabelImpl<NetDirectionLabel>, Counter>::default();
    registry.register("net_bytes", "num net_bytes", net_bytes.clone());

    register_metric_cleaner(proxy_traffic.clone(), 2);
    // register_metric_cleaner(http_req_counter.clone(), 7 * 24);

    Metrics {
        http_req_counter,
        proxy_traffic,
        net_bytes,
    }
}

// 每两小时清空一次，否则一直累积，光是exporter的流量就很大，观察到每天需要3.7GB。不用担心rate函数不准，promql查询会自动处理reset（数据突降）的数据。
// 不过，虽然能够处理reset，但increase会用最后一个出现的值-第一个出现的值。在我们清空的实现下，reset后第一个出现的值肯定不是0，所以increase的算出来的值会稍少（少第一次出现的值）
// 因此对于准确性要求较高的http_req_counter，这里的清空间隔就放大一点
fn register_metric_cleaner<T: Label + Send + Sync>(
    counter: Family<T, Counter>,
    interval_in_hour: u64,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval_in_hour * 60 * 60)).await;
            counter.clear();
        }
    });
}

pub(crate) fn check_auth(
    config_basic_auth: &HashMap<String, String>,
    req: &Request<impl Body>,
    client_socket_addr: &SocketAddr,
    header_name: HeaderName,
) -> (String, bool) {
    let mut username = "unkonwn".to_string();
    let mut authed: bool = true;
    if !config_basic_auth.is_empty() {
        //需要检验鉴权
        authed = false;
        let header_name_clone = header_name.clone();
        let header_name_str = header_name_clone.as_str();
        match req.headers().get(header_name) {
            None => warn!(
                "no {} from {}",
                header_name_str,
                SocketAddrFormat(client_socket_addr)
            ),
            Some(header) => match header.to_str() {
                Err(e) => warn!("解header失败，{:?} {:?}", header, e),
                Ok(request_auth) => match config_basic_auth.get(request_auth) {
                    Some(_username) => {
                        authed = true;
                        username = _username.to_string();
                    }
                    None => warn!(
                        "wrong {} from {}, wrong:{:?},right:{:?}",
                        header_name_str,
                        SocketAddrFormat(client_socket_addr),
                        request_auth,
                        config_basic_auth
                    ),
                },
            },
        }
    }
    (username, authed)
}
// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(
    upgraded: Upgraded,
    target_io: CounterIO<TcpStream, LabelImpl<AccessLabel>>,
) -> io::Result<()> {
    let mut upgraded = TokioIo::new(upgraded);
    let timed_target_io = TimeoutIO::new(target_io, Duration::from_secs(crate::IDLE_SECONDS));
    pin!(timed_target_io);
    // https://github.com/sfackler/tokio-io-timeout/issues/12
    // timed_target_io.as_mut() // 一定要as_mut()，否则会move所有权
    // ._set_timeout_pinned(Duration::from_secs(crate::IDLE_SECONDS));
    let (_from_client, _from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut timed_target_io).await?;
    Ok(())
}
/// Returns the host and port of the given URI.
fn authority(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|authority| authority.to_string())
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReqLabels {
    // Use your own enum types to represent label values.
    pub referer: String,
    // Or just a plain string.
    pub path: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AccessLabel {
    pub client: String,
    pub target: String,
    pub username: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct NetDirectionLabel {
    pub direction: &'static str,
}

impl Display for AccessLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}", self.client, self.target)
    }
}

pub(crate) fn build_authenticate_resp(for_proxy: bool) -> Response<BoxBody<Bytes, io::Error>> {
    let mut resp = Response::new(full_body("auth need"));
    resp.headers_mut().append(
        if for_proxy {
            http::header::PROXY_AUTHENTICATE
        } else {
            http::header::WWW_AUTHENTICATE
        },
        HeaderValue::from_static("Basic realm=\"are you kidding me\""),
    );
    if for_proxy {
        *resp.status_mut() = http::StatusCode::PROXY_AUTHENTICATION_REQUIRED;
    } else {
        *resp.status_mut() = http::StatusCode::UNAUTHORIZED;
    }
    resp
}

pub fn empty_body() -> BoxBody<Bytes, io::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

pub fn full_body<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, io::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}
