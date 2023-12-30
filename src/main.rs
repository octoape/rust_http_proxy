#![deny(warnings)]
mod acceptor;
mod async_io_mod;
mod log_x;
mod net_monitor;
mod proxy;
mod tls_helper;
mod web_func;

use crate::log_x::init_log;
use crate::tls_helper::tls_config;
use acceptor::TlsAcceptor;
use clap::Parser;
use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Error, Request, Response};
use hyper_util::rt::tokio::TokioIo;
use hyper_util::server::conn::auto;
use log::{debug, info, warn};
use proxy::ProxyHandler;
use std::error::Error as stdError;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;
use std::{env, io};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time;
use tokio_rustls::rustls::ServerConfig;

const REFRESH_SECONDS: u64 = 60 * 60; // 1 hour

type DynError = Box<dyn stdError>; // wrapper for dyn Error

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let proxy_config: &'static ProxyConfig = load_config();
    serve(proxy_config).await?;
    Ok(())
}

async fn serve(config: &'static ProxyConfig) -> Result<(), DynError> {
    let proxy_handler = ProxyHandler::new().await;
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let mut terminate_signal = signal(SignalKind::terminate())?;
    if config.over_tls {
        info!("init mine TlsAcceptor");
        let mut acceptor = TlsAcceptor::new(
            tls_config(&config.key, &config.cert)?,
            TcpListener::bind(addr).await?,
        );
        let mut rx = init_tls_config_refresh_task(config);
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl_c => shutdowning");
                    std::process::exit(0); // 并不优雅关闭
                },
                _ = terminate_signal.recv()=>{
                    info!("rust_http_proxy is shutdowning");
                    std::process::exit(0); // 并不优雅关闭
                },
                conn = acceptor.accept() => {
                    match conn {
                        Ok((conn,client_socket_addr)) => {
                            let io = TokioIo::new(conn);
                            let proxy_handler=proxy_handler.clone();
                            tokio::spawn(async move {
                                let binding =auto::Builder::new(hyper_util::rt::tokio::TokioExecutor::new());
                                let connection =
                                    binding.serve_connection_with_upgrades(io, service_fn(move |req| {
                                        proxy(
                                            req,
                                            config,
                                            client_socket_addr,
                                            proxy_handler.clone()
                                        )
                                    }));
                                if let Err(err) = connection.await {
                                     handle_hyper_error(client_socket_addr,err);
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
                    info!("tls config is updated");
                    // Replace the acceptor with the new one
                    acceptor.replace_config(new_config);
                }
            }
        }
    } else {
        let tcp_listener = TcpListener::bind(addr).await?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl_c => shutdowning");
                    std::process::exit(0); // 并不优雅关闭
                },
                _ = terminate_signal.recv()=>{
                    info!("rust_http_proxy is shutdowning");
                    std::process::exit(0); // 并不优雅关闭
                },
                conn = tcp_listener.accept()=>{
                    if let Ok((tcp_stream, client_socket_addr)) =conn{
                        let io = TokioIo::new(tcp_stream);
                        let proxy_handler=proxy_handler.clone();
                        tokio::task::spawn(async move {
                            let connection = http1::Builder::new()
                                .serve_connection(
                                    io,
                                    service_fn(move |req| {
                                        proxy(
                                            req,
                                            config,
                                            client_socket_addr,
                                            proxy_handler.clone()
                                        )
                                    }),
                                )
                                .with_upgrades();
                            if let Err(http_err) = connection.await {
                                handle_hyper_error(client_socket_addr, Box::new(http_err));
                            }
                        });
                    }
                }
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
    config: &'static ProxyConfig,
    client_socket_addr: SocketAddr,
    proxy_handler: ProxyHandler,
) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
    proxy_handler.proxy(req, config, client_socket_addr).await
}

fn log_config(config: &ProxyConfig) {
    info!("log is output to {}/{}", config.log_dir, config.log_file);
    info!("hostname seems to be {}", config.hostname);
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
    info!("basic auth is \"{}\"", config.basic_auth);
    if config.basic_auth.contains('\"') || config.basic_auth.contains('\'') {
        warn!("basic_auth contains quotation marks, please check if it is a mistake!")
    }
    info!(
        "Listening on http{}://{}:{}",
        match config.over_tls {
            true => "s",
            false => "",
        },
        local_ip().unwrap_or("0.0.0.0".to_string()),
        config.port
    );
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
            debug!(
                "[hyper system error]: {:?} [client:{}]",
                cause, client_socket_addr
            )
        }
    } else {
        warn!(
            "[hyper other error]: {} [client:{}]",
            http_err, client_socket_addr
        );
    }
}

fn load_config() -> &'static ProxyConfig {
    let config = Box::leak(Box::new(ProxyConfig::parse()));
    config.hostname = env::var("HOSTNAME").unwrap_or("未知".to_string());
    if let Err(log_init_error) = init_log(&config.log_dir, &config.log_file) {
        println!("init log error:{}", log_init_error);
        std::process::exit(1);
    }
    log_config(config);
    return Box::leak(Box::new(config));
}

pub fn local_ip() -> io::Result<String> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    socket
        .local_addr()
        .map(|local_addr| local_addr.ip().to_string())
}

fn init_tls_config_refresh_task(config: &'static ProxyConfig) -> mpsc::Receiver<Arc<ServerConfig>> {
    let (tx, rx) = mpsc::channel::<Arc<ServerConfig>>(1);
    tokio::spawn(async move {
        info!("update tls config every {} seconds", REFRESH_SECONDS);
        loop {
            time::sleep(Duration::from_secs(REFRESH_SECONDS)).await;
            if let Ok(new_acceptor) = tls_config(&config.key, &config.cert) {
                info!("update tls config");
                tx.try_send(new_acceptor).ok(); // 防止阻塞
            }
        }
    });
    rx
}

/// A HTTP proxy server based on Hyper and Rustls, which features TLS proxy and static file serving.
#[derive(Parser)]
#[command(author, version=None, about, long_about = None)]
pub struct ProxyConfig {
    #[arg(long, value_name = "LOG_DIR", default_value = "/tmp")]
    log_dir: String,
    #[arg(long, value_name = "LOG_FILE", default_value = "proxy.log")]
    log_file: String,
    #[arg(short, long, value_name = "PORT", default_value = "3128")]
    port: u16,
    #[arg(short, long, value_name = "CERT", default_value = "cert.pem")]
    cert: String,
    #[arg(short, long, value_name = "KEY", default_value = "privkey.pem")]
    key: String,
    #[arg(short, long, value_name = "BASIC_AUTH", default_value = "",help="默认为空，表示不鉴权。\n\
    格式为 'Basic Base64Encode(username:password)'，注意username和password用英文冒号连接再进行Base64编码（RFC 7617）。\n\
    例如 'Basic dXNlcm5hbWU6cGFzc3dvcmQ=' \n\
    这由此命令生成： echo -n 'username:passwrod' | base64\n")]
    basic_auth: String,
    #[arg(
        short,
        long,
        value_name = "WEB_CONTENT_PATH",
        default_value = "/usr/share/nginx/html"
    )]
    web_content_path: String,
    #[arg(short, long, value_name = "REFERER", default_value = "",help="Http Referer请求头处理 \n\
    1. 图片资源的防盗链：针对png/jpeg/jpg等文件的请求，要求Request的Referer header要么为空，要么配置的值\n\
    2. 外链访问监控：如果Referer不包含配置的值，并且访问html资源时，Prometheus counter req_from_out++，用于外链访问监控\n")]
    referer: String,
    #[arg(long, value_name = "ASK_FOR_AUTH",help="if enable, never send '407 Proxy Authentication Required' to client。\n\
    建议开启，否则有被嗅探的风险\n")]
    never_ask_for_auth: bool,
    #[arg(short, long, value_name = "OVER_TLS",help="if enable, proxy server will listen on https")]
    over_tls: bool,
    #[arg(long, value_name = "HOSTNAME", default_value = "未知")]
    hostname: String,
}
