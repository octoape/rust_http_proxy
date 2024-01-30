#![deny(warnings)]
mod acceptor;
mod counter_io;
mod log_x;
mod net_monitor;
mod prom_label;
mod proxy;
mod tls_helper;
mod web_func;

use crate::log_x::init_log;
use crate::tls_helper::tls_config;
use acceptor::TlsAcceptor;
use base64::engine::general_purpose;
use base64::Engine;
use clap::Parser;
use futures_util::future::join_all;
use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Error, Request, Response};
use hyper_util::rt::tokio::TokioIo;
use hyper_util::server::conn::auto;
use lazy_static::lazy_static;
use log::{debug, info, warn};
use proxy::ProxyHandler;
use std::collections::HashMap;
use std::error::Error as stdError;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::process::exit;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{env, io};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;
use tokio::time::{self, Instant};
use tokio_rustls::rustls::ServerConfig;
const REFRESH_SECONDS: u64 = 60 * 60; // 1 hour
const IDLE_SECONDS: u64 = if !cfg!(debug_assertions) { 120 } else { 5 }; // 3 minutes

type DynError = Box<dyn stdError>; // wrapper for dyn Error

lazy_static! {
    static ref PROXY_HANDLER: ProxyHandler = ProxyHandler::new();
}

pub struct Context {
    pub instant: Instant,
    pub upgraded: bool,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            instant: Instant::now(),
            upgraded: false,
        }
    }
}

impl Context {
    pub fn refresh(&mut self) {
        self.instant = Instant::now();
    }
}

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let proxy_config: &'static Config = load_config();
    if let Err(e) = handle_signal() {
        warn!("handle signal error:{}", e);
        exit(1)
    }

    let futures = proxy_config
        .port
        .iter()
        .map(|port| {
            let proxy_handler = PROXY_HANDLER.clone();
            async move {
                if let Err(e) = serve(proxy_config, *port, proxy_handler).await {
                    warn!("serve error:{}", e);
                    exit(1)
                }
            }
        })
        .collect::<Vec<_>>();
    join_all(futures).await;
    Ok(())
}

async fn serve(
    config: &'static Config,
    port: u16,
    proxy_handler: ProxyHandler,
) -> Result<(), DynError> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(
        "Listening on http{}://{}:{}",
        match config.over_tls {
            true => "s",
            false => "",
        },
        local_ip().unwrap_or("0.0.0.0".to_string()),
        port
    );
    if config.over_tls {
        let mut acceptor = TlsAcceptor::new(
            tls_config(&config.key, &config.cert)?,
            TcpListener::bind(addr).await?,
        );

        let mut rx = match &config.tls_config_broadcast {
            Some(tls_config_broadcast) => tls_config_broadcast.subscribe(),
            None => {
                warn!("no tls config broadcast channel");
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "no tls config broadcast channel",
                )
                .into());
            }
        };
        loop {
            tokio::select! {
                conn = acceptor.accept() => {
                    match conn {
                        Ok((conn,client_socket_addr)) => {
                            let io = TokioIo::new(conn);
                            let proxy_handler=proxy_handler.clone();
                            tokio::spawn(async move {
                                let binding =auto::Builder::new(hyper_util::rt::tokio::TokioExecutor::new());
                                let context=Arc::new(RwLock::new(Context::default()));
                                let context_c=context.clone();
                                let connection =
                                    binding.serve_connection_with_upgrades(io, service_fn(move |req| {
                                        proxy(
                                            req,
                                            config,
                                            client_socket_addr,
                                            proxy_handler.clone(),
                                            context.clone()
                                        )
                                    }));
                                tokio::pin!(connection);
                                loop{
                                    let last_instant = context_c.read().unwrap().instant;
                                    tokio::select! {
                                        res = connection.as_mut() => {
                                            if let Err(err)=res{
                                                handle_hyper_error(client_socket_addr, err);
                                            }
                                            break;
                                        }
                                        _ = tokio::time::sleep_until(last_instant+Duration::from_secs(IDLE_SECONDS)) => {
                                            let upgraded;
                                            let instant;
                                            {
                                                let context = context_c.read().unwrap();
                                                upgraded = context.upgraded;
                                                instant = context.instant;
                                            }
                                            if !upgraded && instant <= last_instant {
                                                info!("idle for {} seconds, graceful_shutdown [{}]",IDLE_SECONDS,client_socket_addr);
                                                connection.as_mut().graceful_shutdown();
                                                break;
                                            }
                                            if upgraded {
                                                context_c.write().unwrap().refresh();
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Err(err) => {
                            warn!("Error accepting connection: {}", err);
                        }
                    }
                },
                message = rx.recv() => {
                    let new_config = message.expect("Channel should not be closed");
                    info!("tls config is updated for port:{}",port);
                    // Replace the acceptor with the new one
                    acceptor.replace_config(new_config);
                }
            }
        }
    } else {
        let tcp_listener = TcpListener::bind(addr).await?;
        loop {
            if let Ok((tcp_stream, client_socket_addr)) = tcp_listener.accept().await {
                let io = TokioIo::new(tcp_stream);
                let proxy_handler = proxy_handler.clone();
                tokio::task::spawn(async move {
                    let context = Arc::new(RwLock::new(Context::default()));
                    let context_c = context.clone();
                    let connection = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req| {
                                proxy(
                                    req,
                                    config,
                                    client_socket_addr,
                                    proxy_handler.clone(),
                                    context.clone(),
                                )
                            }),
                        )
                        .with_upgrades();
                    tokio::pin!(connection);
                    loop {
                        let last_instant = context_c.read().unwrap().instant;
                        tokio::select! {
                            res = connection.as_mut() => {
                                if let Err(err)=res{
                                    handle_hyper_error(client_socket_addr, Box::new(err));
                                }
                                break;
                            }
                            _ = tokio::time::sleep_until(last_instant+Duration::from_secs(IDLE_SECONDS)) => {
                                let upgraded;
                                let instant;
                                {
                                    let context = context_c.read().unwrap();
                                    upgraded = context.upgraded;
                                    instant = context.instant;
                                }
                                if !upgraded && instant <= last_instant {
                                    info!("idle for {} seconds, graceful_shutdown [{}]",IDLE_SECONDS,client_socket_addr);
                                    connection.as_mut().graceful_shutdown();
                                    break;
                                }
                                if upgraded {
                                    context_c.write().unwrap().refresh();
                                }
                            }
                        }
                    }
                });
            }
        }
    }
}

/// 代理请求
/// # Arguments
/// * `req` - hyper::Request
/// * `config` - 全局配置
/// * `client_socket_addr` - 客户端socket地址
/// * `proxy_handler` - 代理处理器
/// # Returns
/// * `Result<Response<BoxBody<Bytes, io::Error>>, io::Error>` - hyper::Response
async fn proxy(
    req: Request<hyper::body::Incoming>,
    config: &'static Config,
    client_socket_addr: SocketAddr,
    proxy_handler: ProxyHandler,
    context: Arc<RwLock<Context>>,
) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
    match context.write() {
        Ok(mut context) => {
            context.refresh();
        }
        Err(err) => warn!("write context error:{}", err),
    }
    proxy_handler
        .proxy(req, config, client_socket_addr, context)
        .await
}
fn log_config(config: &Config) {
    if !config.basic_auth.is_empty() && !config.never_ask_for_auth {
        warn!("do not serve web content to avoid being detected!");
    } else {
        info!("serve web content of \"{}\"", config.web_content_path);
        if !config.referer.is_empty() {
            info!(
                "Referer header to images must contain \"{}\"",
                config.referer
            );
        }
    }
    info!("basic auth is {:?}", config.basic_auth);
}

/// 处理hyper错误
/// # Arguments
/// * `client_socket_addr` - 客户端socket地址
/// * `http_err` - hyper错误
/// # Returns
/// * `()` - 无返回值
fn handle_hyper_error(client_socket_addr: SocketAddr, http_err: DynError) {
    if let Some(http_err) = http_err.downcast_ref::<Error>() {
        // 转换为hyper::Error
        let cause = match http_err.source() {
            None => http_err,
            Some(e) => e, // 解析cause
        };
        if http_err.is_user() {
            // 判断是否是用户错误
            warn!(
                "[hyper user error]: {:?} [client:{}]",
                cause, client_socket_addr
            );
        } else {
            // 系统错误
            #[cfg(debug_assertions)]
            {
                // 在 debug 模式下执行
                warn!(
                    "[hyper system error]: {:?} [client:{}]",
                    cause, client_socket_addr
                );
            }
            #[cfg(not(debug_assertions))]
            {
                // 在 release 模式下执行
                debug!(
                    "[hyper system error]: {:?} [client:{}]",
                    cause, client_socket_addr
                );
            }
        }
    } else {
        warn!(
            "[hyper other error]: {} [client:{}]",
            http_err, client_socket_addr
        );
    }
}

fn handle_signal() -> io::Result<()> {
    let mut terminate_signal = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate_signal.recv() => {
                info!("receive terminate signal, exit");
                std::process::exit(0);
            },
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl_c => shutdowning");
                std::process::exit(0); // 并不优雅关闭
            },
        };
    });
    Ok(())
}
fn load_config() -> &'static Config {
    let mut config = ProxyConfig::parse();
    config.hostname = env::var("HOSTNAME").unwrap_or("未知".to_string());
    if let Err(log_init_error) = init_log(&config.log_dir, &config.log_file) {
        println!("init log error:{}", log_init_error);
        std::process::exit(1);
    }
    info!("log is output to {}/{}", config.log_dir, config.log_file);
    info!("hostname seems to be {}", config.hostname);
    let config = Config::from(config);
    log_config(&config);
    info!(
        "auto close connection after idle for {} seconds",
        IDLE_SECONDS
    );
    return Box::leak(Box::new(config));
}

pub fn local_ip() -> io::Result<String> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    socket
        .local_addr()
        .map(|local_addr| local_addr.ip().to_string())
}

/// A HTTP proxy server based on Hyper and Rustls, which features TLS proxy and static file serving.
#[derive(Parser)]
#[command(author, version=None, about, long_about = None)]
pub struct ProxyConfig {
    #[arg(long, value_name = "LOG_DIR", default_value = "/tmp")]
    log_dir: String,
    #[arg(long, value_name = "LOG_FILE", default_value = "proxy.log")]
    log_file: String,
    #[arg(
        short,
        long,
        value_name = "PORT",
        default_value = "3128",
        help = "可以多次指定来实现多端口\n"
    )]
    port: Vec<u16>,
    #[arg(short, long, value_name = "CERT", default_value = "cert.pem")]
    cert: String,
    #[arg(short, long, value_name = "KEY", default_value = "privkey.pem")]
    key: String,
    #[arg(
        short,
        long,
        value_name = "USER",
        default_value = "",
        help = "默认为空，表示不鉴权。\n\
    格式为 'username:password'\n\
    可以多次指定来实现多用户\n"
    )]
    users: Vec<String>,
    #[arg(
        short,
        long,
        value_name = "WEB_CONTENT_PATH",
        default_value = "/usr/share/nginx/html"
    )]
    web_content_path: String,
    #[arg(
        short,
        long,
        value_name = "REFERER",
        default_value = "",
        help = "Http Referer请求头处理 \n\
    1. 图片资源的防盗链：针对png/jpeg/jpg等文件的请求，要求Request的Referer header要么为空，要么配置的值\n\
    2. 外链访问监控：如果Referer不包含配置的值，并且访问html资源时，Prometheus counter req_from_out++，用于外链访问监控\n"
    )]
    referer: String,
    #[arg(
        long,
        value_name = "ASK_FOR_AUTH",
        help = "if enable, never send '407 Proxy Authentication Required' to client。\n\
    建议开启，否则有被嗅探的风险\n"
    )]
    never_ask_for_auth: bool,
    #[arg(
        short,
        long,
        value_name = "OVER_TLS",
        help = "if enable, proxy server will listen on https"
    )]
    over_tls: bool,
    #[arg(long, value_name = "HOSTNAME", default_value = "未知")]
    hostname: String,
}

pub(crate) struct Config {
    cert: String,
    key: String,
    basic_auth: HashMap<String, String>,
    web_content_path: String,
    referer: String,
    never_ask_for_auth: bool,
    over_tls: bool,
    hostname: String,
    port: Vec<u16>,
    tls_config_broadcast: Option<broadcast::Sender<Arc<ServerConfig>>>,
}

impl From<ProxyConfig> for Config {
    fn from(config: ProxyConfig) -> Self {
        let mut basic_auth = HashMap::new();
        for raw_user in config.users {
            let mut user = raw_user.split(':');
            let username = user.next().unwrap_or("").to_string();
            let password = user.next().unwrap_or("").to_string();
            if !username.is_empty() && !password.is_empty() {
                let base64 = general_purpose::STANDARD.encode(raw_user);
                basic_auth.insert(format!("Basic {}", base64), username);
            }
        }
        let tls_config_broadcast = if config.over_tls {
            let (tx, _rx) = broadcast::channel::<Arc<ServerConfig>>(10);
            let tx_clone = tx.clone();
            let key_clone = config.key.clone();
            let cert_clone = config.cert.clone();
            tokio::spawn(async move {
                info!("update tls config every {} seconds", REFRESH_SECONDS);
                loop {
                    time::sleep(Duration::from_secs(REFRESH_SECONDS)).await;
                    if let Ok(new_acceptor) = tls_config(&key_clone, &cert_clone) {
                        info!("update tls config");
                        if let Err(e) = tx_clone.send(new_acceptor) {
                            warn!("send tls config error:{}", e);
                        }
                    }
                }
            });
            Some(tx)
        } else {
            None
        };
        debug!("");
        Config {
            cert: config.cert,
            key: config.key,
            basic_auth,
            web_content_path: config.web_content_path,
            referer: config.referer,
            never_ask_for_auth: config.never_ask_for_auth,
            over_tls: config.over_tls,
            hostname: config.hostname,
            port: config.port,
            tls_config_broadcast,
        }
    }
}
